//! `groundcontrol run-exec <run-id>`: executes one recorded run in the
//! foreground of its tmux window (docs/redesign-daemonless.md, "Pipeline runs").
//!
//! The worker holds the project's run lock for the whole run, so a project
//! never has two runs at once. Step output goes, masked, to the pane and to
//! the step log, which the TUI follows. The TUI answers approvals and
//! requests cancels through the store; the worker polls both. C-c, SIGTERM
//! and SIGHUP (the window closed) cancel the run too.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::Instant;

use super::{APPROVAL_TTL, approval_vars, expand_message, fmt_secs, gc_vars, parse_outputs, step_env};
use crate::config::{Config, Pipeline, Project, ServiceAction, StepKind};
use crate::mask::Masker;
use crate::process;
use crate::protocol::*;
use crate::services::worker::{Signals, close_own_pane, pane, say};
use crate::store::Store;

const TICK: Duration = Duration::from_millis(250);
/// How long a succeeded run's window stays open before it closes itself.
const LINGER: Duration = Duration::from_secs(5);

struct Exec {
    store: Store,
    id: i64,
    signals: Signals,
    cancelled: bool,
    next_check: Instant,
}

impl Exec {
    /// Runs a store write; a failure is reported in the pane and otherwise ignored.
    fn db<T>(&self, f: impl FnOnce(&Store) -> rusqlite::Result<T>) -> Option<T> {
        match f(&self.store) {
            Ok(v) => Some(v),
            Err(e) => {
                say(&format!("cannot record state: {e}"));
                None
            }
        }
    }

    /// Resolves once the run is cancelled: a signal, or the store's cancel flag.
    /// The flag check keeps its own schedule, so a step that prints constantly
    /// (and so keeps restarting the `select!` around this) cannot starve it.
    async fn until_cancelled(&mut self) {
        while !self.cancelled {
            let at = self.next_check;
            tokio::select! {
                _ = self.signals.recv() => self.cancelled = true,
                _ = tokio::time::sleep_until(at) => {
                    self.next_check = Instant::now() + TICK;
                    if self.store.run_cancel_requested(self.id).unwrap_or(false) {
                        self.cancelled = true;
                    }
                }
            }
        }
    }

    fn check_cancelled(&mut self) -> bool {
        if !self.cancelled && self.store.run_cancel_requested(self.id).unwrap_or(false) {
            self.cancelled = true;
        }
        self.cancelled
    }
}

enum StepOutcome {
    Ok,
    Failed(Option<String>),
    Rejected,
    Cancelled(Option<String>),
}

/// Entry point. Returns the process exit code: 0 when the run succeeded.
pub async fn run(id: i64) -> i32 {
    let state = crate::paths::state_dir();
    if let Err(e) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&state) {
        say(&format!("cannot create {}: {e}", state.display()));
        return 1;
    }
    let store = match Store::open(&state.join(crate::paths::DB_FILE)) {
        Ok(s) => s,
        Err(e) => {
            say(&format!("cannot open the state database: {e}"));
            return 1;
        }
    };
    let detail = match store.run_detail(id) {
        Ok(Some(d)) => d,
        _ => {
            say(&format!("run #{id} is not recorded"));
            return 1;
        }
    };
    if !detail.summary.status.is_active() {
        say(&format!("run #{id} has already finished"));
        return 1;
    }
    let summary = detail.summary;
    let fail = |store: &Store, msg: &str| {
        say(msg);
        let _ = store.interrupt_run(id, RunStatus::Failed, msg, now());
        pane(b"\x07");
        1
    };
    let lock = match crate::lock::acquire(&crate::paths::run_lock_path(&state, &summary.project)) {
        Ok(Some(l)) => l,
        Ok(None) => {
            let code = fail(&store, &format!("{} already has a run in progress", summary.project));
            close_own_pane().await;
            return code;
        }
        Err(e) => return fail(&store, &format!("cannot take the project's run lock: {e}")),
    };
    let signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => return fail(&store, &format!("cannot install signal handlers: {e}")),
    };
    let Some(config) = crate::config::load_file(&crate::paths::config_path()).config else {
        return fail(&store, "groundcontrol.yaml is invalid; see the Health screen");
    };
    let Some(project) = config.project(&summary.project) else {
        return fail(&store, &format!("project `{}` is no longer in the config", summary.project));
    };
    let Some(pipeline) = project.pipeline(&summary.pipeline) else {
        return fail(&store, &format!("pipeline `{}` is no longer in {}", summary.pipeline, project.id));
    };
    if pipeline.steps.len() != detail.steps.len() {
        return fail(&store, "the pipeline changed since the run was started; start it again");
    }

    say(&format!("run #{id}: {}/{}{}", project.id, pipeline.id, summary.env.as_deref().map(|e| format!(" on {e}")).unwrap_or_default()));
    let mut exec = Exec { store, id, signals, cancelled: false, next_check: Instant::now() };
    let (status, reason) = execute(&mut exec, &config, project, pipeline, summary.env.as_deref(), &detail.inputs).await;

    let text = format!("run #{id} {}{}", status.as_str(), reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default());
    say(&text);
    drop(lock);
    if status == RunStatus::Succeeded {
        tokio::time::sleep(LINGER).await;
        close_own_pane().await;
        0
    } else {
        if status == RunStatus::Failed {
            pane(b"\x07");
        }
        1
    }
}

async fn execute(
    exec: &mut Exec,
    config: &Config,
    project: &Project,
    pipeline: &Pipeline,
    env: Option<&str>,
    inputs: &BTreeMap<String, String>,
) -> (RunStatus, Option<String>) {
    let id = exec.id;
    let tmp = crate::paths::run_tmp_dir(id);
    let out_file = tmp.join("outputs");
    if let Err(e) = std::fs::create_dir_all(&tmp).and_then(|_| std::fs::write(&out_file, "")) {
        let reason = format!("cannot prepare run folder: {e}");
        exec.db(|s| s.interrupt_run(id, RunStatus::Failed, &reason, now()));
        return (RunStatus::Failed, Some(reason));
    }
    let mut outputs: BTreeMap<String, String> = BTreeMap::new();
    let mut outputs_seen = 0usize;
    let mut final_status = RunStatus::Succeeded;
    let mut reason: Option<String> = None;
    let gc = gc_vars(Some(id), env, &out_file);
    let mut all_secrets: Vec<String> = Vec::new();
    let total = pipeline.steps.len();

    for (idx, step) in pipeline.steps.iter().enumerate() {
        if exec.check_cancelled() {
            final_status = RunStatus::Cancelled;
            reason = Some("cancelled".into());
            break;
        }
        say(&format!("step {}/{total}: {}", idx + 1, step.label()));
        let outcome = match &step.kind {
            StepKind::Run { run, cwd, env_file, timeout } => {
                let dir = project.resolve_dir(cwd.as_deref());
                let env_file_path = env_file.as_ref().map(|f| dir.join(f));
                match step_env(config, project, env, env_file_path.as_deref(), inputs, &outputs, &gc) {
                    Err(e) => {
                        fail_step_early(exec, project, idx, &e);
                        StepOutcome::Failed(Some(e))
                    }
                    Ok((vars, secrets)) => {
                        all_secrets.extend(secrets.iter().cloned());
                        run_step(exec, project, idx, run, &dir, &vars, Masker::new(secrets), *timeout).await
                    }
                }
            }
            StepKind::Approve { message } => {
                let text = expand_message(message, &approval_vars(config, project, env, inputs, &outputs, &gc));
                approve_step(exec, idx, &text).await
            }
            StepKind::Service { action, target } => service_step(exec, config, project, idx, *action, target).await,
        };

        // Collect outputs appended during this step (§6.4).
        if let Ok(text) = std::fs::read_to_string(&out_file) {
            let masker = Masker::new(&all_secrets);
            for (k, v) in parse_outputs(&text).into_iter().skip(outputs_seen) {
                outputs_seen += 1;
                let v = String::from_utf8_lossy(&masker.mask_all(v.as_bytes())).into_owned();
                exec.db(|s| s.set_output(id, &k, &v, idx));
                outputs.insert(k, v);
            }
        }

        match outcome {
            StepOutcome::Ok => {}
            StepOutcome::Failed(r) => {
                final_status = RunStatus::Failed;
                reason = r;
            }
            StepOutcome::Rejected => final_status = RunStatus::Rejected,
            StepOutcome::Cancelled(r) => {
                final_status = RunStatus::Cancelled;
                reason = r;
            }
        }
        if final_status != RunStatus::Succeeded {
            break;
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    exec.db(|s| s.skip_pending_steps(id));
    exec.db(|s| s.set_run_status(id, final_status, reason.as_deref(), Some(now())));
    (final_status, reason)
}

/// Writes masked bytes to the step log and the pane.
struct StepLog {
    file: Option<std::fs::File>,
}

impl StepLog {
    fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if let Some(f) = &mut self.file {
            let _ = f.write_all(data);
        }
        pane(data);
    }

    fn finish(&mut self) {
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
    }
}

/// Opens the step's log file and records the step as running.
fn begin_step(exec: &Exec, project: &Project, idx: usize, status: StepStatus) -> Result<StepLog, String> {
    let path = crate::paths::step_log_path(&project.data_root(), exec.id, idx);
    let opened = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&path));
    let path_text = path.to_string_lossy().into_owned();
    match opened {
        Ok(f) => {
            exec.db(|s| s.start_step(exec.id, idx, now(), status, Some(&path_text)));
            Ok(StepLog { file: Some(f) })
        }
        Err(e) => {
            exec.db(|s| s.start_step(exec.id, idx, now(), status, None));
            Err(format!("cannot open step log {}: {e}", PathBuf::from(&path_text).display()))
        }
    }
}

fn fail_step_early(exec: &Exec, project: &Project, idx: usize, msg: &str) {
    let mut log = begin_step(exec, project, idx, StepStatus::Running).unwrap_or(StepLog { file: None });
    log.write(format!("groundcontrol: {msg}\n").as_bytes());
    log.finish();
    exec.db(|s| s.finish_step(exec.id, idx, StepStatus::Failed, None, now()));
}

#[allow(clippy::too_many_arguments)]
async fn run_step(
    exec: &mut Exec,
    project: &Project,
    idx: usize,
    command: &str,
    dir: &Path,
    vars: &BTreeMap<String, String>,
    mut masker: Masker,
    timeout: Duration,
) -> StepOutcome {
    if !dir.is_dir() {
        let msg = format!("working directory {} does not exist", dir.display());
        fail_step_early(exec, project, idx, &msg);
        return StepOutcome::Failed(Some(msg));
    }
    let mut log = match begin_step(exec, project, idx, StepStatus::Running) {
        Ok(l) => l,
        Err(msg) => {
            say(&msg);
            exec.db(|s| s.finish_step(exec.id, idx, StepStatus::Failed, None, now()));
            return StepOutcome::Failed(Some(msg));
        }
    };
    let mut spawned = match process::spawn_shell(command, dir, vars) {
        Ok(s) => s,
        Err(e) => {
            log.write(format!("groundcontrol: cannot start step: {e}\n").as_bytes());
            log.finish();
            exec.db(|s| s.finish_step(exec.id, idx, StepStatus::Failed, None, now()));
            return StepOutcome::Failed(Some(format!("cannot start step: {e}")));
        }
    };
    let pgid = spawned.pid;
    let deadline = Instant::now() + timeout;
    enum End {
        Exited(Option<i32>),
        TimedOut,
        Cancelled,
    }
    let mut output_open = true;
    let end = loop {
        tokio::select! {
            chunk = spawned.output.recv(), if output_open => match chunk {
                Some(c) => log.write(&masker.push(&c)),
                None => output_open = false,
            },
            status = spawned.child.wait() => break End::Exited(status.ok().and_then(|s| s.code())),
            _ = tokio::time::sleep_until(deadline) => break End::TimedOut,
            _ = exec.until_cancelled() => break End::Cancelled,
        }
    };
    let code = match end {
        End::Exited(c) => c,
        End::TimedOut | End::Cancelled => {
            let what = if matches!(end, End::TimedOut) { "timed out; stopping" } else { "cancel requested; stopping" };
            log.write(&masker.push(format!("\ngroundcontrol: {what} (SIGTERM, then SIGKILL after 10s)\n").as_bytes()));
            process::terminate(&mut spawned.child, pgid).await
        }
    };
    // Drain what is left; something the step left running may hold the pipe open.
    let drain_until = Instant::now() + Duration::from_secs(2);
    while output_open {
        match tokio::time::timeout_at(drain_until, spawned.output.recv()).await {
            Ok(Some(c)) => log.write(&masker.push(&c)),
            _ => output_open = false,
        }
    }
    log.write(&masker.flush());
    log.finish();

    let (status, outcome) = match end {
        End::TimedOut => {
            (StepStatus::TimedOut, StepOutcome::Failed(Some(format!("step {} timed out after {}", idx + 1, fmt_secs(timeout.as_secs())))))
        }
        End::Cancelled => (StepStatus::Cancelled, StepOutcome::Cancelled(Some("cancelled".into()))),
        End::Exited(Some(0)) => (StepStatus::Succeeded, StepOutcome::Ok),
        End::Exited(c) => (
            StepStatus::Failed,
            StepOutcome::Failed(Some(match c {
                Some(c) => format!("step {} exited with code {c}", idx + 1),
                None => format!("step {} was killed by a signal", idx + 1),
            })),
        ),
    };
    exec.db(|s| s.finish_step(exec.id, idx, status, code, now()));
    outcome
}

/// Pauses for a decision recorded by the TUI. Expires after 30 minutes.
async fn approve_step(exec: &mut Exec, idx: usize, message: &str) -> StepOutcome {
    let id = exec.id;
    let requested = now();
    let expires = requested + APPROVAL_TTL.as_secs() as i64;
    exec.db(|s| s.insert_approval(id, idx, message, requested, expires));
    exec.db(|s| s.start_step(id, idx, requested, StepStatus::Waiting, None));
    exec.db(|s| s.set_run_status(id, RunStatus::AwaitingApproval, None, None));
    say(&format!("waiting for approval in GroundControl: {message}"));
    pane(b"\x07");

    let deadline = Instant::now() + APPROVAL_TTL;
    let decided = loop {
        tokio::select! {
            _ = exec.until_cancelled() => break None,
            _ = tokio::time::sleep_until(deadline) => break None,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                match exec.store.approval(id, idx) {
                    Ok(Some(a)) if a.status != ApprovalStatus::Pending => break Some(a.status),
                    _ => {}
                }
            }
        }
    };
    let at = now();
    // Expired or cancelled: record it unless an answer landed at the same moment.
    let status = match decided {
        Some(s) => s,
        None => {
            let mine = if exec.cancelled { ApprovalStatus::Cancelled } else { ApprovalStatus::Expired };
            match exec.db(|s| s.decide_approval(id, idx, mine, at)) {
                Some(Err(existing)) => existing,
                _ => mine,
            }
        }
    };
    match status {
        ApprovalStatus::Approved => {
            say("approved");
            exec.db(|s| s.finish_step(id, idx, StepStatus::Succeeded, None, at));
            exec.db(|s| s.set_run_status(id, RunStatus::Running, None, None));
            StepOutcome::Ok
        }
        ApprovalStatus::Rejected => {
            say("rejected");
            exec.db(|s| s.finish_step(id, idx, StepStatus::Rejected, None, at));
            StepOutcome::Rejected
        }
        _ => {
            exec.db(|s| s.finish_step(id, idx, StepStatus::Cancelled, None, at));
            StepOutcome::Cancelled(Some(if exec.cancelled { "cancelled".into() } else { "approval expired".into() }))
        }
    }
}

async fn service_step(exec: &mut Exec, config: &Config, project: &Project, idx: usize, action: ServiceAction, target: &str) -> StepOutcome {
    let mut log = begin_step(exec, project, idx, StepStatus::Running).unwrap_or(StepLog { file: None });
    log.write(format!("groundcontrol: {} {target}…\n", action.as_str()).as_bytes());
    let result = match crate::ctx::Ctx::from_env() {
        Ok(ctx) => crate::services::control(&ctx, config, &project.id, target, action).await,
        Err(e) => Err(e),
    };
    let (status, outcome) = match result {
        Ok(msg) => {
            log.write(format!("groundcontrol: {msg}\n").as_bytes());
            (StepStatus::Succeeded, StepOutcome::Ok)
        }
        Err(e) => {
            log.write(format!("groundcontrol: {e}\n").as_bytes());
            (StepStatus::Failed, StepOutcome::Failed(Some(e)))
        }
    };
    log.finish();
    exec.db(|s| s.finish_step(exec.id, idx, status, None, now()));
    outcome
}
