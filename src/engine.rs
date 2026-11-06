//! The TUI's in-process engine (docs/redesign-daemonless.md, "Live updates in
//! the TUI"). It answers the same `Request`s the old daemon's socket did, and
//! turns what tmux-hosted workers write (the store, step and service logs)
//! into the same `Event`s by polling. It owns no runs and no services:
//! quitting the TUI stops only the engine.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::{Config, StepKind};
use crate::ctx::Ctx;
use crate::protocol::*;

const POLL: Duration = Duration::from_millis(250);
const GIT_POLL: Duration = Duration::from_secs(30);
const REAP_EVERY: Duration = Duration::from_secs(5);
/// Docker is checked only while the Health screen is open, at most this often.
const DOCKER_EVERY: Duration = Duration::from_secs(30);
const RETENTION_DAYS: u64 = 90;
const MAX_LOG_READ: u64 = 1024 * 1024;
const MAX_TEXT: u64 = 4 * 1024 * 1024;

/// Tools the steps are expected to find on `PATH` (§6.6).
pub const REQUIRED_TOOLS: [&str; 9] = ["git", "gh", "pnpm", "npm", "cargo", "go", "docker", "tmux", "sqlite3"];

#[derive(Debug, Clone)]
pub struct RequestError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn rerr(code: ErrorCode, message: impl Into<String>) -> RequestError {
    RequestError { code, message: message.into() }
}

pub struct Engine {
    pub ctx: Ctx,
    started_at: Timestamp,
    config: RwLock<Arc<Config>>,
    config_status: Mutex<ConfigStatus>,
    git: Mutex<HashMap<String, GitStatus>>,
    git_refresh: mpsc::UnboundedSender<Option<String>>,
    topics: Mutex<HashSet<String>>,
    events: mpsc::UnboundedSender<Event>,
    docker: Mutex<Option<(Instant, Reachability)>>,
}

impl Engine {
    /// Opens the state, loads the config and starts the pollers. Call inside
    /// a tokio runtime.
    pub fn open(ctx: Ctx) -> (Arc<Engine>, mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        let (git_tx, git_rx) = mpsc::unbounded_channel();
        let e = Arc::new(Engine {
            started_at: now(),
            config_status: Mutex::new(ConfigStatus {
                path: ctx.config_path.display().to_string(),
                ok: false,
                active: false,
                diagnostics: Vec::new(),
                last_attempt_at: None,
                loaded_at: None,
            }),
            ctx,
            config: RwLock::new(Arc::new(Config::default())),
            git: Mutex::new(HashMap::new()),
            git_refresh: git_tx,
            topics: Mutex::new(HashSet::new()),
            events,
            docker: Mutex::new(None),
        });
        e.reload_config_now();
        tokio::spawn(watch_config(e.clone()));
        tokio::spawn(git_poller(e.clone(), git_rx));
        tokio::spawn(state_poller(e.clone()));
        tokio::spawn(reaper(e.clone()));
        tokio::spawn(retention(e.clone()));
        (e, rx)
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap().clone()
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn subscribed(&self, topic: &str) -> bool {
        self.topics.lock().unwrap().contains(topic)
    }

    /// Loads the config file; keeps the previous good config when it is invalid.
    fn reload_config_now(&self) {
        let result = crate::config::load_file(&self.ctx.config_path);
        let t = now();
        let ok = result.config.is_some();
        let status = {
            let mut st = self.config_status.lock().unwrap();
            st.ok = ok;
            st.diagnostics = result.diagnostics.clone();
            st.last_attempt_at = Some(t);
            if let Some(c) = result.config {
                *self.config.write().unwrap() = Arc::new(c);
                st.active = true;
                st.loaded_at = Some(t);
            }
            st.clone()
        };
        if ok {
            self.emit(Event::Projects);
            let _ = self.git_refresh.send(None);
        }
        self.emit(Event::Config(status));
    }

    pub fn config_status(&self) -> ConfigStatus {
        self.config_status.lock().unwrap().clone()
    }

    /// Answers one request, the way the daemon's socket server did.
    pub async fn request(&self, req: Request) -> Result<Response, RequestError> {
        let db_err = |e: String| rerr(ErrorCode::Internal, e);
        match req {
            Request::ProjectsList => Ok(Response::Projects(self.projects())),
            Request::RunStart(p) => {
                let project = p.project.clone();
                let r = crate::runs::start(&self.ctx, &self.config(), p).await.map_err(|(c, m)| rerr(c, m));
                if r.is_ok() {
                    self.emit(Event::Projects);
                    let _ = self.git_refresh.send(Some(project));
                }
                r
            }
            Request::RunCancel { id } => crate::runs::cancel(&self.ctx, id).map_err(|(c, m)| rerr(c, m)),
            Request::RunDecide { id, step, decision, confirmed } => {
                crate::runs::decide(&self.ctx, id, step, decision, confirmed).map_err(|(c, m)| rerr(c, m))
            }
            Request::RunsQuery { filter, page } => self.ctx.db(|s| s.query_runs(&filter, page)).map(Response::Runs).map_err(db_err),
            Request::RunGet { id } => match self.ctx.db(|s| s.run_detail(id)).map_err(db_err)? {
                Some(r) => Ok(Response::Run(r)),
                None => Err(rerr(ErrorCode::NotFound, format!("run #{id} not found"))),
            },
            Request::RunDiff { id } => {
                let detail = self
                    .ctx
                    .db(|s| s.run_detail(id))
                    .ok()
                    .flatten()
                    .ok_or_else(|| rerr(ErrorCode::NotFound, format!("run #{id} not found")))?;
                let path = detail
                    .diff_path
                    .ok_or_else(|| rerr(ErrorCode::NotFound, format!("run #{id} has no saved diff (the tree was clean)")))?;
                let text = read_capped(Path::new(&path), MAX_TEXT)
                    .map_err(|e| rerr(ErrorCode::NotFound, format!("cannot read {path}: {e} (diffs are pruned after 90 days)")))?;
                Ok(Response::Text { text, path: Some(path) })
            }
            Request::LogRead { run, step, from_offset } => self.log_read(run, step, from_offset),
            Request::ServiceControl { project, id, action } => {
                let r = crate::services::control(&self.ctx, &self.config(), &project, &id, action).await;
                r.map(|msg| Response::Text { text: msg, path: None }).map_err(|e| rerr(ErrorCode::Refused, e))
            }
            Request::ServiceLog { project, id, max_bytes } => {
                let config = self.config();
                let p = config.project(&project).ok_or_else(|| rerr(ErrorCode::NotFound, format!("unknown project `{project}`")))?;
                if p.service(&id).is_none() {
                    return Err(rerr(ErrorCode::NotFound, format!("unknown service `{id}`")));
                }
                let path = self.service_log_path(p, &id);
                let text = match crate::logfile::tail(&path, max_bytes.min(MAX_TEXT)) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return Err(rerr(ErrorCode::Internal, format!("cannot read service log: {e}"))),
                };
                Ok(Response::Text { text, path: Some(path.display().to_string()) })
            }
            Request::HealthGet => Ok(Response::Health(self.health().await)),
            Request::Subscribe { topics } => {
                *self.topics.lock().unwrap() = topics.into_iter().collect();
                Ok(Response::Ok)
            }
        }
    }

    fn service_log_path(&self, p: &crate::config::Project, service: &str) -> PathBuf {
        if p.sensitive {
            crate::paths::service_log_path(&p.id, &p.path, true, service)
        } else {
            self.ctx.state_dir.join("services").join(&p.id).join(format!("{service}.log"))
        }
    }

    pub fn projects(&self) -> Vec<ProjectInfo> {
        let config = self.config();
        let git = self.git.lock().unwrap().clone();
        let waiting = self.ctx.db(|s| s.waiting_approvals()).unwrap_or_default();
        config
            .projects
            .iter()
            .map(|p| ProjectInfo {
                id: p.id.clone(),
                path: p.path_display.clone(),
                remote: p.remote.clone(),
                sensitive: p.sensitive,
                disabled: p.disabled.clone(),
                busy_run: crate::runs::busy_run(&self.ctx, &p.id).filter(|id| *id > 0),
                git: git.get(&p.id).cloned(),
                environments: p
                    .environments
                    .iter()
                    .map(|e| EnvInfo {
                        name: e.name.clone(),
                        protected: e.protected,
                        var_names: e.vars.iter().map(|(k, _)| k.clone()).collect(),
                    })
                    .collect(),
                services: p.services.iter().map(|s| crate::services::status(&self.ctx, p, s)).collect(),
                groups: p.groups.iter().map(|g| GroupInfo { id: g.id.clone(), members: g.members.clone() }).collect(),
                pipelines: p
                    .pipelines
                    .iter()
                    .map(|pl| PipelineInfo {
                        id: pl.id.clone(),
                        protected: pl.protected,
                        environments: pl.environments.clone(),
                        inputs: pl
                            .inputs
                            .iter()
                            .map(|i| InputInfo {
                                name: i.name.clone(),
                                kind: i.kind,
                                pattern: i.pattern.clone(),
                                options: i.options.clone(),
                                default: i.default.clone(),
                            })
                            .collect(),
                        steps: pl
                            .steps
                            .iter()
                            .map(|s| StepOutline {
                                kind: StepKindName::parse(s.kind_str()),
                                label: s.label(),
                                detail: match &s.kind {
                                    StepKind::Run { run, .. } => run.clone(),
                                    StepKind::Approve { message } => message.clone(),
                                    StepKind::Service { action, target } => format!("{} {target}", action.as_str()),
                                },
                            })
                            .collect(),
                        warning: pl.warning.clone(),
                        last_run: self.ctx.db(|s| s.last_run(&p.id, &pl.id)).ok().flatten(),
                        waiting: waiting.iter().filter(|a| a.project == p.id && a.pipeline == pl.id).count() as u32,
                    })
                    .collect(),
            })
            .collect()
    }

    /// Tools are resolved on `PATH` in-process. tmux is asked for its version;
    /// Docker only while the Health screen is open.
    pub async fn health(&self) -> HealthInfo {
        let tools = REQUIRED_TOOLS
            .iter()
            .map(|t| {
                let path = find_on_path(t);
                ToolStatus { name: t.to_string(), ok: path.is_some(), path: path.map(|p| p.display().to_string()) }
            })
            .collect();
        let tmux = Some(match self.ctx.tmux.version().await {
            Ok(v) => Reachability { ok: true, detail: v },
            Err(e) => Reachability { ok: false, detail: e },
        });
        let docker = if self.subscribed("health") {
            let cached = self.docker.lock().unwrap().clone().filter(|(at, _)| at.elapsed() < DOCKER_EVERY).map(|(_, r)| r);
            match cached {
                Some(r) => Some(r),
                None => {
                    let r = docker_reachable().await;
                    *self.docker.lock().unwrap() = Some((Instant::now(), r.clone()));
                    Some(r)
                }
            }
        } else {
            self.docker.lock().unwrap().clone().map(|(_, r)| r)
        };
        HealthInfo {
            tools,
            tools_checked_at: Some(now()),
            docker,
            tmux,
            config: self.config_status(),
            version: VERSION.to_string(),
            started_at: self.started_at,
            state_dir: self.ctx.state_dir.display().to_string(),
        }
    }

    fn log_read(&self, run: i64, step: usize, from: u64) -> Result<Response, RequestError> {
        let info = self
            .ctx
            .db(|s| s.step(run, step))
            .ok()
            .flatten()
            .ok_or_else(|| rerr(ErrorCode::NotFound, format!("run #{run} has no step {}", step + 1)))?;
        let finished = !matches!(info.status, StepStatus::Running | StepStatus::Pending | StepStatus::Waiting);
        let Some(path) = self.ctx.db(|s| s.step_log_path(run, step)).ok().flatten() else {
            return Ok(Response::Log(LogChunk { run, step, offset: from, data: String::new(), end: from, complete: finished }));
        };
        match read_chunk(Path::new(&path), from) {
            Ok((offset, data, end, len)) => Ok(Response::Log(LogChunk { run, step, offset, data, end, complete: finished && end >= len })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let data = if finished { "(log pruned after 90 days)\n".to_string() } else { String::new() };
                Ok(Response::Log(LogChunk { run, step, offset: from, data, end: from, complete: finished }))
            }
            Err(e) => Err(rerr(ErrorCode::Internal, format!("cannot read log: {e}"))),
        }
    }
}

fn read_capped(path: &Path, max: u64) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    (&mut f).take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Reads up to 1 MB from `from`, never splitting a UTF-8 sequence. Returns
/// `(offset, text, end, file length)`.
fn read_chunk(path: &Path, from: u64) -> std::io::Result<(u64, String, u64, u64)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let from = from.min(len);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    (&mut f).take(MAX_LOG_READ).read_to_end(&mut buf)?;
    let cut = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => buf.len(),
    };
    buf.truncate(cut);
    let end = from + buf.len() as u64;
    Ok((from, String::from_utf8_lossy(&buf).into_owned(), end, len))
}

/// Looks a command up on `PATH` without running anything.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

async fn docker_reachable() -> Reachability {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["info", "--format", "{{.ServerVersion}}"]).stdin(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(8), cmd.output()).await {
        Err(_) => Reachability { ok: false, detail: "timed out".into() },
        Ok(Err(e)) => Reachability { ok: false, detail: format!("cannot run docker: {e}") },
        Ok(Ok(o)) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if v.is_empty() {
                Reachability { ok: false, detail: "no server version reported".into() }
            } else {
                Reachability { ok: true, detail: format!("server {v}") }
            }
        }
        Ok(Ok(o)) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Reachability { ok: false, detail: err.lines().find(|l| !l.trim().is_empty()).unwrap_or("docker failed").trim().to_string() }
        }
    }
}

/// Watches the config file's folder (editors replace files on save) and reloads.
async fn watch_config(e: Arc<Engine>) {
    use notify::Watcher;
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let name = e.ctx.config_path.file_name().map(|n| n.to_os_string());
    let Ok(mut watcher) = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev.paths.iter().any(|p| p.file_name().map(|n| n.to_os_string()) == name)
        {
            let _ = tx.send(());
        }
    }) else {
        return;
    };
    let dir = e.ctx.config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    if watcher.watch(&dir, notify::RecursiveMode::NonRecursive).is_err() {
        return;
    }
    while rx.recv().await.is_some() {
        // Debounce bursts of events from a single save.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while rx.try_recv().is_ok() {}
        let e2 = e.clone();
        let _ = tokio::task::spawn_blocking(move || e2.reload_config_now()).await;
    }
}

async fn git_poller(e: Arc<Engine>, mut rx: mpsc::UnboundedReceiver<Option<String>>) {
    let mut tick = tokio::time::interval(GIT_POLL);
    loop {
        let only = tokio::select! {
            _ = tick.tick() => None,
            r = rx.recv() => match r { Some(p) => p, None => return },
        };
        let config = e.config();
        for p in config.projects.iter().filter(|p| only.as_ref().is_none_or(|o| o == &p.id)) {
            if p.disabled.is_some() {
                e.git.lock().unwrap().remove(&p.id);
                continue;
            }
            let status = crate::git::status(&p.path, p.remote.as_deref()).await;
            let changed = {
                let mut cache = e.git.lock().unwrap();
                let changed = cache.get(&p.id).is_none_or(|old| GitStatus { checked_at: status.checked_at, ..old.clone() } != status);
                cache.insert(p.id.clone(), status.clone());
                changed
            };
            if changed {
                e.emit(Event::Git { project: p.id.clone(), git: status });
            }
        }
    }
}

async fn reaper(e: Arc<Engine>) {
    loop {
        if !crate::runs::reap(&e.ctx).await.is_empty() {
            e.emit(Event::Projects);
        }
        tokio::time::sleep(REAP_EVERY).await;
    }
}

/// What the poller last reported, to emit only changes. Values are JSON so
/// any protocol type can be compared.
#[derive(Default)]
struct Seen {
    primed: bool,
    runs: HashMap<i64, String>,
    active: HashSet<i64>,
    steps: HashMap<(i64, usize), String>,
    approvals: HashMap<(i64, usize), String>,
    outputs: HashMap<(i64, String), String>,
    services: HashMap<(String, String), String>,
    busy: HashMap<String, Option<i64>>,
    /// Step log offsets for subscribed runs; `true` once `complete` was sent.
    logs: HashMap<(i64, usize), (u64, bool)>,
    service_logs: HashMap<(String, String), u64>,
}

fn changed<K: std::hash::Hash + Eq>(map: &mut HashMap<K, String>, key: K, value: &impl serde::Serialize) -> bool {
    let json = serde_json::to_string(value).unwrap_or_default();
    match map.get(&key) {
        Some(old) if *old == json => false,
        _ => {
            map.insert(key, json);
            true
        }
    }
}

/// Turns store and log-file changes into events (the daemon pushed these).
async fn state_poller(e: Arc<Engine>) {
    let mut seen = Seen::default();
    let mut tick = 0u64;
    loop {
        poll_runs(&e, &mut seen);
        poll_logs(&e, &mut seen);
        if tick.is_multiple_of(2) {
            poll_services(&e, &mut seen);
        }
        seen.primed = true;
        tick += 1;
        tokio::time::sleep(POLL).await;
    }
}

fn poll_runs(e: &Engine, seen: &mut Seen) {
    let Ok(page) = e.ctx.db(|s| s.query_runs(&RunFilter::default(), 0)) else { return };
    let mut detail_ids: HashSet<i64> = HashSet::new();
    let mut any_change = false;
    for r in &page.runs {
        if changed(&mut seen.runs, r.id, r) {
            any_change = true;
            detail_ids.insert(r.id);
            if seen.primed {
                e.emit(Event::Run(r.clone()));
            }
        }
    }
    let active: HashSet<i64> = page.runs.iter().filter(|r| r.status.is_active()).map(|r| r.id).collect();
    // Runs that were active last time get one more look at their final steps.
    detail_ids.extend(active.iter().copied());
    detail_ids.extend(seen.active.iter().copied());
    for t in e.topics.lock().unwrap().iter() {
        if let Some(id) = t.strip_prefix("log:").and_then(|s| s.parse::<i64>().ok()) {
            detail_ids.insert(id);
        }
    }
    seen.active = active;
    for id in detail_ids {
        let Ok(Some(d)) = e.ctx.db(|s| s.run_detail(id)) else { continue };
        for st in &d.steps {
            if changed(&mut seen.steps, (id, st.index), st) && seen.primed {
                e.emit(Event::Step { run_id: id, step: st.clone() });
            }
        }
        for a in &d.approvals {
            if changed(&mut seen.approvals, (id, a.step_index), a) && seen.primed {
                e.emit(Event::Approval(a.clone()));
            }
        }
        for o in &d.outputs {
            if changed(&mut seen.outputs, (id, o.key.clone()), &o.value) && seen.primed {
                e.emit(Event::Output { run_id: id, key: o.key.clone(), value: o.value.clone() });
            }
        }
    }
    let config = e.config();
    for p in &config.projects {
        let busy = crate::runs::busy_run(&e.ctx, &p.id);
        if seen.busy.insert(p.id.clone(), busy) != Some(busy) {
            any_change = true;
        }
    }
    if any_change && seen.primed {
        e.emit(Event::Projects);
    }
}

fn poll_services(e: &Engine, seen: &mut Seen) {
    let config = e.config();
    for p in &config.projects {
        for s in &p.services {
            let st = crate::services::status(&e.ctx, p, s);
            if changed(&mut seen.services, (p.id.clone(), s.id.clone()), &st) && seen.primed {
                e.emit(Event::Service(st));
            }
        }
    }
}

/// Follows the step logs of subscribed runs and the subscribed service logs.
fn poll_logs(e: &Engine, seen: &mut Seen) {
    let topics: Vec<String> = e.topics.lock().unwrap().iter().cloned().collect();
    let runs: HashSet<i64> = topics.iter().filter_map(|t| t.strip_prefix("log:")?.parse().ok()).collect();
    seen.logs.retain(|(run, _), _| runs.contains(run));
    for run in runs {
        let Ok(Some(d)) = e.ctx.db(|s| s.run_detail(run)) else { continue };
        for st in &d.steps {
            let Ok(Some(path)) = e.ctx.db(|s| s.step_log_path(run, st.index)) else { continue };
            let finished = !matches!(st.status, StepStatus::Running | StepStatus::Pending | StepStatus::Waiting);
            // From the start: a fast step may have finished writing before
            // this poll saw its log. The run view drops what it already has.
            let (offset, done) = seen.logs.get(&(run, st.index)).copied().unwrap_or((0, false));
            let key = (run, st.index);
            if done {
                continue;
            }
            let Ok((from, data, end, len)) = read_chunk(Path::new(&path), offset) else { continue };
            if !data.is_empty() {
                e.emit(Event::Log(LogChunk { run, step: st.index, offset: from, data, end, complete: false }));
            }
            let complete = finished && end >= len;
            if complete {
                e.emit(Event::Log(LogChunk { run, step: st.index, offset: end, data: String::new(), end, complete: true }));
            }
            seen.logs.insert(key, (end, complete));
        }
    }

    let services: Vec<(String, String)> = topics
        .iter()
        .filter_map(|t| t.strip_prefix("service-log:")?.split_once('/'))
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect();
    seen.service_logs.retain(|k, _| services.contains(k));
    let config = e.config();
    for (pid, sid) in services {
        let Some(p) = config.project(&pid) else { continue };
        let path = e.service_log_path(p, &sid);
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let key = (pid.clone(), sid.clone());
        let offset = match seen.service_logs.get(&key) {
            None => {
                seen.service_logs.insert(key, len);
                continue;
            }
            // Rotated: start over in the new file.
            Some(&o) if o > len => 0,
            Some(&o) => o,
        };
        if let Ok((_, data, end, _)) = read_chunk(&path, offset)
            && !data.is_empty()
        {
            e.emit(Event::ServiceLog { project: pid, service: sid, data });
            seen.service_logs.insert(key, end);
        }
    }
}

/// Prunes run logs and saved diffs older than 90 days (§11), once per start.
async fn retention(e: Arc<Engine>) {
    let mut roots = vec![e.ctx.state_dir.clone()];
    roots.extend(e.config().projects.iter().filter(|p| p.sensitive).map(|p| p.data_root()));
    let _ = tokio::task::spawn_blocking(move || {
        let cutoff = std::time::SystemTime::now() - Duration::from_secs(RETENTION_DAYS * 24 * 3600);
        for root in roots {
            prune_older(&root.join("logs"), cutoff);
            prune_older(&root.join("diffs"), cutoff);
            prune_older(&root.join("tmp"), std::time::SystemTime::now() - Duration::from_secs(7 * 24 * 3600));
        }
    })
    .await;
}

fn prune_older(dir: &Path, cutoff: std::time::SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.modified().is_ok_and(|m| m < cutoff) {
            let ok = if meta.is_dir() { std::fs::remove_dir_all(e.path()) } else { std::fs::remove_file(e.path()) };
            if ok.is_ok() {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tools_without_running_them() {
        assert!(find_on_path("sh").is_some());
        assert!(find_on_path("definitely-not-a-tool-gc").is_none());
    }

    #[test]
    fn chunks_never_split_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l");
        std::fs::write(&p, "aé").unwrap(); // `é` is 2 bytes
        let (from, data, end, len) = read_chunk(&p, 0).unwrap();
        assert_eq!((from, data.as_str(), end, len), (0, "aé", 3, 3));
        let (_, data, end, _) = read_chunk(&p, 3).unwrap();
        assert_eq!((data.as_str(), end), ("", 3));
    }
}
