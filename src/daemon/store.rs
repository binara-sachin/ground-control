//! SQLite store (§11). The daemon, the TUI and the tmux-hosted workers each
//! open their own connection (WAL, with a busy timeout). Secrets and step
//! output never enter it.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::protocol::*;

const SCHEMA_VERSION: i64 = 2;
pub const PAGE_SIZE: u32 = 200;

pub struct Store {
    conn: Connection,
}

pub struct NewRun<'a> {
    pub project: &'a str,
    pub pipeline: &'a str,
    pub env: Option<&'a str>,
    pub inputs: &'a BTreeMap<String, String>,
    pub protected: bool,
    pub branch: Option<&'a str>,
    pub commit: Option<&'a str>,
    pub dirty: bool,
    pub unpushed: bool,
    pub unpushed_count: Option<u32>,
}

pub struct NewStep {
    pub name: String,
    pub kind: StepKindName,
    pub command: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredService {
    pub project: String,
    pub service: String,
    pub desired_running: bool,
    pub pid: Option<i32>,
    pub pgid: Option<i32>,
    pub started_at: Option<Timestamp>,
    pub restart_count: u32,
    pub last_exit_code: Option<i32>,
    pub status: ServiceState,
    pub healthy: bool,
    /// Short explanation shown next to the state ("needs api", "port 8787 is in use").
    pub message: Option<String>,
    /// Token of the `svc-exec` launch that wrote the row, so a starter can tell
    /// its own launch from a leftover row.
    pub launch: Option<String>,
    /// Set by the store on every save; ignored when saving.
    pub updated_at: Option<Timestamp>,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> rusqlite::Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Store> {
        // Before anything else: several processes may open the file at once.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project TEXT NOT NULL,
                pipeline TEXT NOT NULL,
                env TEXT,
                inputs TEXT NOT NULL DEFAULT '{}',
                branch TEXT,
                commit_sha TEXT,
                dirty INTEGER NOT NULL DEFAULT 0,
                unpushed INTEGER NOT NULL DEFAULT 0,
                unpushed_count INTEGER,
                diff_path TEXT,
                diff_summary TEXT,
                protected INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                reason TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS runs_by_project ON runs(project, pipeline, id);
            CREATE INDEX IF NOT EXISTS runs_by_status ON runs(status);
            CREATE TABLE IF NOT EXISTS steps (
                run_id INTEGER NOT NULL REFERENCES runs(id),
                idx INTEGER NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                command TEXT NOT NULL,
                cwd TEXT,
                status TEXT NOT NULL,
                exit_code INTEGER,
                started_at INTEGER,
                finished_at INTEGER,
                log_path TEXT,
                PRIMARY KEY (run_id, idx)
            );
            CREATE TABLE IF NOT EXISTS approvals (
                run_id INTEGER NOT NULL REFERENCES runs(id),
                step_index INTEGER NOT NULL,
                message TEXT NOT NULL,
                status TEXT NOT NULL,
                requested_at INTEGER NOT NULL,
                decided_at INTEGER,
                expires_at INTEGER NOT NULL,
                PRIMARY KEY (run_id, step_index)
            );
            CREATE TABLE IF NOT EXISTS outputs (
                run_id INTEGER NOT NULL REFERENCES runs(id),
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                step_index INTEGER,
                PRIMARY KEY (run_id, key)
            );
            CREATE TABLE IF NOT EXISTS service_state (
                project TEXT NOT NULL,
                service TEXT NOT NULL,
                desired_state TEXT NOT NULL,
                pid INTEGER,
                pgid INTEGER,
                started_at INTEGER,
                restart_count INTEGER NOT NULL DEFAULT 0,
                last_exit_code INTEGER,
                status TEXT NOT NULL,
                PRIMARY KEY (project, service)
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        // v2: service rows written by tmux-hosted workers.
        for (col, ty) in [
            ("healthy", "INTEGER NOT NULL DEFAULT 0"),
            ("message", "TEXT"),
            ("launch", "TEXT"),
            ("updated_at", "INTEGER"),
            ("stop_requested", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            add_column(&conn, "service_state", col, ty)?;
        }
        let store = Store { conn };
        store.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(store)
    }

    // ── meta ──

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    // ── runs ──

    pub fn insert_run(&self, r: &NewRun, steps: &[NewStep], started_at: Timestamp) -> rusqlite::Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO runs (project, pipeline, env, inputs, branch, commit_sha, dirty, unpushed, unpushed_count, protected, status, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'running', ?11)",
            params![
                r.project,
                r.pipeline,
                r.env,
                serde_json::to_string(r.inputs).unwrap_or_else(|_| "{}".into()),
                r.branch,
                r.commit,
                r.dirty,
                r.unpushed,
                r.unpushed_count,
                r.protected,
                started_at
            ],
        )?;
        let id = tx.last_insert_rowid();
        for (i, s) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO steps (run_id, idx, name, kind, command, cwd, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
                params![id, i as i64, s.name, s.kind.as_str(), s.command, s.cwd],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn set_run_diff(&self, id: i64, path: &str, summary: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE runs SET diff_path = ?2, diff_summary = ?3 WHERE id = ?1", params![id, path, summary])?;
        Ok(())
    }

    pub fn set_run_status(&self, id: i64, status: RunStatus, reason: Option<&str>, finished_at: Option<Timestamp>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = ?2, reason = ?3, finished_at = ?4 WHERE id = ?1",
            params![id, status.as_str(), reason, finished_at],
        )?;
        Ok(())
    }

    pub fn start_step(&self, run: i64, idx: usize, at: Timestamp, status: StepStatus, log_path: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE steps SET status = ?3, started_at = ?4, log_path = ?5 WHERE run_id = ?1 AND idx = ?2",
            params![run, idx as i64, status.as_str(), at, log_path],
        )?;
        Ok(())
    }

    pub fn set_step_status(&self, run: i64, idx: usize, status: StepStatus) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE steps SET status = ?3 WHERE run_id = ?1 AND idx = ?2", params![run, idx as i64, status.as_str()])?;
        Ok(())
    }

    pub fn finish_step(&self, run: i64, idx: usize, status: StepStatus, exit_code: Option<i32>, at: Timestamp) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE steps SET status = ?3, exit_code = ?4, finished_at = ?5 WHERE run_id = ?1 AND idx = ?2",
            params![run, idx as i64, status.as_str(), exit_code, at],
        )?;
        Ok(())
    }

    /// Marks every step that has not started as skipped.
    pub fn skip_pending_steps(&self, run: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE steps SET status = 'skipped' WHERE run_id = ?1 AND status = 'pending'", [run])?;
        Ok(())
    }

    pub fn step_log_path(&self, run: i64, idx: usize) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT log_path FROM steps WHERE run_id = ?1 AND idx = ?2", params![run, idx as i64], |r| r.get(0))
            .optional()
            .map(Option::flatten)
    }

    pub fn step(&self, run: i64, idx: usize) -> rusqlite::Result<Option<StepInfo>> {
        self.conn
            .query_row(
                "SELECT idx, name, kind, command, cwd, status, exit_code, started_at, finished_at, log_path FROM steps WHERE run_id = ?1 AND idx = ?2",
                params![run, idx as i64],
                step_from_row,
            )
            .optional()
    }

    pub fn run_summary(&self, id: i64) -> rusqlite::Result<Option<RunSummary>> {
        self.conn.query_row(&format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"), [id], summary_from_row).optional()
    }

    pub fn run_detail(&self, id: i64) -> rusqlite::Result<Option<RunDetail>> {
        let Some(summary) = self.run_summary(id)? else { return Ok(None) };
        let (inputs, diff_path, diff_summary, unpushed_count): (String, Option<String>, Option<String>, Option<u32>) =
            self.conn.query_row("SELECT inputs, diff_path, diff_summary, unpushed_count FROM runs WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
        let mut stmt = self.conn.prepare(
            "SELECT idx, name, kind, command, cwd, status, exit_code, started_at, finished_at, log_path FROM steps WHERE run_id = ?1 ORDER BY idx",
        )?;
        let steps = stmt.query_map([id], step_from_row)?.collect::<Result<Vec<_>, _>>()?;
        let approvals = self.approvals_where("a.run_id = ?1", params![id])?;
        let mut stmt = self.conn.prepare("SELECT key, value, step_index FROM outputs WHERE run_id = ?1 ORDER BY rowid")?;
        let outputs = stmt
            .query_map([id], |r| {
                Ok(OutputInfo { key: r.get(0)?, value: r.get(1)?, step_index: r.get::<_, Option<i64>>(2)?.map(|i| i as usize) })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(RunDetail {
            summary,
            inputs: serde_json::from_str(&inputs).unwrap_or_default(),
            steps,
            approvals,
            outputs,
            diff_path,
            diff_summary,
            unpushed_count,
        }))
    }

    pub fn query_runs(&self, filter: &RunFilter, page: u32) -> rusqlite::Result<RunPage> {
        let mut where_ = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if let Some(p) = &filter.project {
            args.push(p.clone());
            where_.push(format!("project = ?{}", args.len()));
        }
        if let Some(p) = &filter.pipeline {
            args.push(p.clone());
            where_.push(format!("pipeline = ?{}", args.len()));
        }
        if let Some(e) = &filter.env {
            if e == "-" || e == "none" {
                where_.push("env IS NULL".into());
            } else {
                args.push(e.clone());
                where_.push(format!("env = ?{}", args.len()));
            }
        }
        if let Some(s) = filter.status {
            args.push(s.as_str().to_string());
            where_.push(format!("status = ?{}", args.len()));
        }
        let clause = if where_.is_empty() { String::new() } else { format!("WHERE {}", where_.join(" AND ")) };
        let total: u64 =
            self.conn.query_row(&format!("SELECT COUNT(*) FROM runs {clause}"), rusqlite::params_from_iter(args.iter()), |r| {
                r.get::<_, i64>(0)
            })? as u64;
        let sql =
            format!("SELECT {RUN_COLS} FROM runs {clause} ORDER BY id DESC LIMIT {PAGE_SIZE} OFFSET {}", page as u64 * PAGE_SIZE as u64);
        let mut stmt = self.conn.prepare(&sql)?;
        let runs = stmt.query_map(rusqlite::params_from_iter(args.iter()), summary_from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(RunPage { runs, total, page, page_size: PAGE_SIZE, waiting: self.waiting_approvals()? })
    }

    pub fn last_run(&self, project: &str, pipeline: &str) -> rusqlite::Result<Option<RunSummary>> {
        self.conn
            .query_row(
                &format!("SELECT {RUN_COLS} FROM runs WHERE project = ?1 AND pipeline = ?2 ORDER BY id DESC LIMIT 1"),
                params![project, pipeline],
                summary_from_row,
            )
            .optional()
    }

    /// Runs left active by a previous daemon process are closed out.
    pub fn recover_interrupted(&self, now: Timestamp) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM runs WHERE status IN ('running', 'awaiting_approval')")?;
        let ids = stmt.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
        for id in &ids {
            self.conn.execute(
                "UPDATE steps SET status = 'cancelled', finished_at = ?2 WHERE run_id = ?1 AND status IN ('running', 'waiting')",
                params![id, now],
            )?;
            self.skip_pending_steps(*id)?;
            self.conn.execute(
                "UPDATE approvals SET status = 'cancelled', decided_at = ?2 WHERE run_id = ?1 AND status = 'pending'",
                params![id, now],
            )?;
            self.set_run_status(*id, RunStatus::Cancelled, Some("daemon restarted"), Some(now))?;
        }
        Ok(ids)
    }

    // ── approvals ──

    pub fn insert_approval(
        &self,
        run: i64,
        step: usize,
        message: &str,
        requested_at: Timestamp,
        expires_at: Timestamp,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO approvals (run_id, step_index, message, status, requested_at, expires_at) VALUES (?1, ?2, ?3, 'pending', ?4, ?5)",
            params![run, step as i64, message, requested_at, expires_at],
        )?;
        Ok(())
    }

    /// Records a decision if the approval is still pending. Returns the status
    /// that was already recorded when another answer won.
    pub fn decide_approval(
        &self,
        run: i64,
        step: usize,
        status: ApprovalStatus,
        at: Timestamp,
    ) -> rusqlite::Result<Result<(), ApprovalStatus>> {
        let changed = self.conn.execute(
            "UPDATE approvals SET status = ?3, decided_at = ?4 WHERE run_id = ?1 AND step_index = ?2 AND status = 'pending'",
            params![run, step as i64, status.as_str(), at],
        )?;
        if changed == 1 {
            return Ok(Ok(()));
        }
        let existing: Option<String> = self
            .conn
            .query_row("SELECT status FROM approvals WHERE run_id = ?1 AND step_index = ?2", params![run, step as i64], |r| r.get(0))
            .optional()?;
        Ok(Err(existing.map(|s| ApprovalStatus::parse(&s)).unwrap_or(ApprovalStatus::Cancelled)))
    }

    pub fn approval(&self, run: i64, step: usize) -> rusqlite::Result<Option<ApprovalInfo>> {
        Ok(self.approvals_where("a.run_id = ?1 AND a.step_index = ?2", params![run, step as i64])?.into_iter().next())
    }

    pub fn waiting_approvals(&self) -> rusqlite::Result<Vec<ApprovalInfo>> {
        self.approvals_where("a.status = 'pending'", params![])
    }

    fn approvals_where(&self, clause: &str, args: impl rusqlite::Params) -> rusqlite::Result<Vec<ApprovalInfo>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT a.run_id, a.step_index, r.project, r.pipeline, r.env, r.protected, a.message, a.status, a.requested_at, a.decided_at, a.expires_at
             FROM approvals a JOIN runs r ON r.id = a.run_id WHERE {clause} ORDER BY a.requested_at"
        ))?;
        stmt.query_map(args, |r| {
            Ok(ApprovalInfo {
                run_id: r.get(0)?,
                step_index: r.get::<_, i64>(1)? as usize,
                project: r.get(2)?,
                pipeline: r.get(3)?,
                env: r.get(4)?,
                protected: r.get(5)?,
                message: r.get(6)?,
                status: ApprovalStatus::parse(&r.get::<_, String>(7)?),
                requested_at: r.get(8)?,
                decided_at: r.get(9)?,
                expires_at: r.get(10)?,
            })
        })?
        .collect()
    }

    // ── outputs ──

    pub fn set_output(&self, run: i64, key: &str, value: &str, step: usize) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO outputs (run_id, key, value, step_index) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id, key) DO UPDATE SET value = excluded.value, step_index = excluded.step_index",
            params![run, key, value, step as i64],
        )?;
        Ok(())
    }

    // ── services ──

    pub fn save_service(&self, s: &StoredService) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO service_state (project, service, desired_state, pid, pgid, started_at, restart_count, last_exit_code, status,
               healthy, message, launch, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(project, service) DO UPDATE SET desired_state = excluded.desired_state, pid = excluded.pid,
               pgid = excluded.pgid, started_at = excluded.started_at, restart_count = excluded.restart_count,
               last_exit_code = excluded.last_exit_code, status = excluded.status, healthy = excluded.healthy,
               message = excluded.message, launch = excluded.launch, updated_at = excluded.updated_at",
            params![
                s.project,
                s.service,
                if s.desired_running { "running" } else { "stopped" },
                s.pid,
                s.pgid,
                s.started_at,
                s.restart_count,
                s.last_exit_code,
                s.status.as_str(),
                s.healthy,
                s.message,
                s.launch,
                now(),
            ],
        )?;
        Ok(())
    }

    pub fn services(&self) -> rusqlite::Result<Vec<StoredService>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {SERVICE_COLS} FROM service_state"))?;
        stmt.query_map([], service_from_row)?.collect()
    }

    pub fn service(&self, project: &str, service: &str) -> rusqlite::Result<Option<StoredService>> {
        self.conn
            .query_row(
                &format!("SELECT {SERVICE_COLS} FROM service_state WHERE project = ?1 AND service = ?2"),
                [project, service],
                service_from_row,
            )
            .optional()
    }

    /// Saves the row a starter writes before opening the worker's window, and
    /// clears any stop request left from an earlier launch.
    pub fn begin_service_launch(&self, s: &StoredService) -> rusqlite::Result<()> {
        self.save_service(s)?;
        self.conn.execute("UPDATE service_state SET stop_requested = 0 WHERE project = ?1 AND service = ?2", [&s.project, &s.service])?;
        Ok(())
    }

    /// Asks the worker holding the service to stop; it polls this flag.
    pub fn request_service_stop(&self, project: &str, service: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE service_state SET stop_requested = 1 WHERE project = ?1 AND service = ?2", [project, service])?;
        Ok(())
    }

    pub fn service_stop_requested(&self, project: &str, service: &str) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT stop_requested FROM service_state WHERE project = ?1 AND service = ?2", [project, service], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some_and(|v| v != 0))
    }

    /// The Mac rebooted: nothing should come back up.
    pub fn clear_desired_services(&self) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE service_state SET desired_state = 'stopped', pid = NULL, pgid = NULL, status = 'stopped'", [])?;
        Ok(())
    }
}

const SERVICE_COLS: &str = "project, service, desired_state, pid, pgid, started_at, restart_count, last_exit_code, status, \
     healthy, message, launch, updated_at";

fn service_from_row(r: &Row) -> rusqlite::Result<StoredService> {
    Ok(StoredService {
        project: r.get(0)?,
        service: r.get(1)?,
        desired_running: r.get::<_, String>(2)? == "running",
        pid: r.get(3)?,
        pgid: r.get(4)?,
        started_at: r.get(5)?,
        restart_count: r.get(6)?,
        last_exit_code: r.get(7)?,
        status: ServiceState::parse(&r.get::<_, String>(8)?),
        healthy: r.get(9)?,
        message: r.get(10)?,
        launch: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

/// `ALTER TABLE … ADD COLUMN` unless the column exists. Two processes opening
/// the store together may both try, so "duplicate column" counts as success.
fn add_column(conn: &Connection, table: &str, col: &str, ty: &str) -> rusqlite::Result<()> {
    let exists = conn.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?.exists([col])?;
    if exists {
        return Ok(());
    }
    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"), []) {
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("duplicate column") => Ok(()),
        r => r.map(|_| ()),
    }
}

const RUN_COLS: &str =
    "id, project, pipeline, env, status, reason, branch, commit_sha, dirty, unpushed, protected, started_at, finished_at";

fn summary_from_row(r: &Row) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        id: r.get(0)?,
        project: r.get(1)?,
        pipeline: r.get(2)?,
        env: r.get(3)?,
        status: RunStatus::parse(&r.get::<_, String>(4)?).unwrap_or(RunStatus::Failed),
        reason: r.get(5)?,
        branch: r.get(6)?,
        commit: r.get(7)?,
        dirty: r.get(8)?,
        unpushed: r.get(9)?,
        protected: r.get(10)?,
        started_at: r.get(11)?,
        finished_at: r.get(12)?,
    })
}

fn step_from_row(r: &Row) -> rusqlite::Result<StepInfo> {
    Ok(StepInfo {
        index: r.get::<_, i64>(0)? as usize,
        name: r.get(1)?,
        kind: StepKindName::parse(&r.get::<_, String>(2)?),
        command: r.get(3)?,
        cwd: r.get(4)?,
        status: StepStatus::parse(&r.get::<_, String>(5)?),
        exit_code: r.get(6)?,
        started_at: r.get(7)?,
        finished_at: r.get(8)?,
        has_log: r.get::<_, Option<String>>(9)?.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_run(s: &Store, project: &str, pipeline: &str, env: Option<&str>) -> i64 {
        let inputs = BTreeMap::from([("pr".to_string(), "12".to_string())]);
        s.insert_run(
            &NewRun {
                project,
                pipeline,
                env,
                inputs: &inputs,
                protected: env.is_some(),
                branch: Some("main"),
                commit: Some("a41c9e2"),
                dirty: false,
                unpushed: false,
                unpushed_count: Some(0),
            },
            &[
                NewStep { name: "Test".into(), kind: StepKindName::Run, command: "pnpm test".into(), cwd: None },
                NewStep { name: "approve".into(), kind: StepKindName::Approve, command: "Deploy?".into(), cwd: None },
            ],
            100,
        )
        .unwrap()
    }

    #[test]
    fn run_lifecycle_and_query() {
        let s = Store::open_in_memory().unwrap();
        let a = new_run(&s, "fa", "deploy", Some("prod"));
        let b = new_run(&s, "fa", "pull", None);
        let c = new_run(&s, "tm", "deploy", None);
        s.set_run_status(b, RunStatus::Failed, None, Some(120)).unwrap();
        s.start_step(a, 0, 101, StepStatus::Running, Some("/x/0.log")).unwrap();
        s.finish_step(a, 0, StepStatus::Succeeded, Some(0), 110).unwrap();
        s.set_output(a, "d1_bookmark", "0000-abc", 0).unwrap();

        let d = s.run_detail(a).unwrap().unwrap();
        assert_eq!(d.summary.status, RunStatus::Running);
        assert_eq!(d.inputs.get("pr").map(String::as_str), Some("12"));
        assert_eq!(d.steps[0].status, StepStatus::Succeeded);
        assert!(d.steps[0].has_log);
        assert_eq!(d.outputs[0].value, "0000-abc");

        let all = s.query_runs(&RunFilter::default(), 0).unwrap();
        assert_eq!(all.total, 3);
        assert_eq!(all.runs[0].id, c, "newest first");
        let f = RunFilter { project: Some("fa".into()), status: Some(RunStatus::Failed), ..Default::default() };
        let page = s.query_runs(&f, 0).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.runs[0].id, b);
        let f = RunFilter { env: Some("-".into()), ..Default::default() };
        assert_eq!(s.query_runs(&f, 0).unwrap().total, 2);
        assert_eq!(s.last_run("fa", "deploy").unwrap().unwrap().id, a);
    }

    #[test]
    fn first_answer_wins() {
        let s = Store::open_in_memory().unwrap();
        let a = new_run(&s, "fa", "deploy", Some("prod"));
        s.insert_approval(a, 1, "Deploy?", 100, 1900).unwrap();
        assert_eq!(s.waiting_approvals().unwrap().len(), 1);
        assert_eq!(s.decide_approval(a, 1, ApprovalStatus::Rejected, 101).unwrap(), Ok(()));
        assert_eq!(s.decide_approval(a, 1, ApprovalStatus::Approved, 102).unwrap(), Err(ApprovalStatus::Rejected));
        assert!(s.waiting_approvals().unwrap().is_empty());
    }

    #[test]
    fn recovery_closes_active_runs() {
        let s = Store::open_in_memory().unwrap();
        let a = new_run(&s, "fa", "deploy", Some("prod"));
        s.start_step(a, 0, 101, StepStatus::Running, None).unwrap();
        assert_eq!(s.recover_interrupted(200).unwrap(), vec![a]);
        let d = s.run_detail(a).unwrap().unwrap();
        assert_eq!(d.summary.status, RunStatus::Cancelled);
        assert_eq!(d.summary.reason.as_deref(), Some("daemon restarted"));
        assert_eq!(d.steps[0].status, StepStatus::Cancelled);
        assert_eq!(d.steps[1].status, StepStatus::Skipped);
    }

    #[test]
    fn opening_twice_migrates_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();
        assert_eq!(b.meta("schema_version").unwrap().as_deref(), Some("2"));
        drop((a, b));
    }

    #[test]
    fn service_state_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let svc = StoredService {
            project: "gi".into(),
            service: "postgres".into(),
            desired_running: true,
            pid: Some(10),
            pgid: Some(10),
            started_at: Some(5),
            restart_count: 1,
            last_exit_code: None,
            status: ServiceState::Running,
            healthy: true,
            message: Some("ok".into()),
            launch: Some("t1".into()),
            updated_at: None,
        };
        s.save_service(&svc).unwrap();
        let back = s.services().unwrap();
        assert!(back[0].desired_running);
        assert_eq!(back[0].status, ServiceState::Running);
        let one = s.service("gi", "postgres").unwrap().unwrap();
        assert!(one.healthy);
        assert_eq!((one.message.as_deref(), one.launch.as_deref()), (Some("ok"), Some("t1")));
        assert!(one.updated_at.is_some());
        assert!(s.service("gi", "nope").unwrap().is_none());

        // Stop requests survive saves and are cleared by the next launch.
        assert!(!s.service_stop_requested("gi", "postgres").unwrap());
        s.request_service_stop("gi", "postgres").unwrap();
        s.save_service(&svc).unwrap();
        assert!(s.service_stop_requested("gi", "postgres").unwrap());
        s.begin_service_launch(&svc).unwrap();
        assert!(!s.service_stop_requested("gi", "postgres").unwrap());
        s.clear_desired_services().unwrap();
        assert!(!s.services().unwrap()[0].desired_running);
        s.set_meta("last_saved_at", "42").unwrap();
        assert_eq!(s.meta("last_saved_at").unwrap().as_deref(), Some("42"));
    }
}
