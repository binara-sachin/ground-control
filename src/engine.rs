//! The TUI's in-process engine (docs/architecture.md, "The TUI"). It answers
//! the TUI's `Request`s and turns what the tmux-hosted workers publish (their
//! windows' `@gc_*` options and pane output) into `Event`s by polling tmux.
//! It owns no runs and no services, and stores nothing: quitting the TUI
//! stops only the engine.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use regex::Regex;
use tokio::sync::mpsc;

use crate::config::{Config, StepKind};
use crate::ctx::Ctx;
use crate::protocol::*;
use crate::window::{self, RunWindow, Windows};

const POLL: Duration = Duration::from_secs(1);
const GIT_POLL: Duration = Duration::from_secs(30);
/// Docker is checked only while the Health screen is open, at most this often.
const DOCKER_EVERY: Duration = Duration::from_secs(30);

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
    /// Loads the config and starts the pollers. Call inside a tokio runtime.
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

    /// Answers one request from the TUI.
    pub async fn request(&self, req: Request) -> Result<Response, RequestError> {
        let tmux_err = |e: String| rerr(ErrorCode::Internal, e);
        let run_err = |(c, m): crate::runs::ReqError| rerr(c, m);
        match req {
            Request::ProjectsList => {
                let windows = window::list(&self.ctx.tmux).await.map_err(tmux_err)?;
                Ok(Response::Projects(self.projects(&windows)))
            }
            Request::RunStart(p) => {
                let project = p.project.clone();
                let r = crate::runs::start(&self.ctx, &self.config(), p).await.map_err(run_err);
                if r.is_ok() {
                    self.emit(Event::Projects);
                    let _ = self.git_refresh.send(Some(project));
                }
                r
            }
            Request::RunCancel { id } => crate::runs::cancel(&self.ctx, id).await.map_err(run_err),
            Request::RunDecide { id, step, decision, confirmed } => {
                crate::runs::decide(&self.ctx, id, step, decision, confirmed).await.map_err(run_err)
            }
            Request::RunsList => {
                let windows = window::list(&self.ctx.tmux).await.map_err(tmux_err)?;
                Ok(Response::Runs(self.run_page(&windows)))
            }
            Request::RunGet { id } => match self.run_window(id).await? {
                Some(w) => Ok(Response::Run(crate::runs::detail(&self.config(), &w))),
                None => Err(rerr(ErrorCode::NotFound, format!("run #{id} has no window any more"))),
            },
            Request::LogRead { run, step } => {
                let Some(w) = self.run_window(run).await? else {
                    return Err(rerr(ErrorCode::NotFound, format!("run #{run} has no window any more")));
                };
                let pane = self.ctx.tmux.capture(run).await.map_err(tmux_err)?;
                let detail = crate::runs::detail(&self.config(), &w);
                Ok(Response::Log(step_chunk(&detail, &split_steps(&pane, latest_started(&detail)), step)))
            }
            Request::ServiceControl { project, id, action } => {
                let r = crate::services::control(&self.ctx, &self.config(), &project, &id, action).await;
                r.map(|msg| Response::Text { text: msg }).map_err(|e| rerr(ErrorCode::Refused, e))
            }
            Request::ServiceLog { project, id } => {
                let windows = window::list(&self.ctx.tmux).await.map_err(tmux_err)?;
                let text = match crate::services::service_windows(&windows, &project, &id).last() {
                    Some(w) => trim_capture(&self.ctx.tmux.capture(w.id).await.map_err(tmux_err)?),
                    None => String::new(),
                };
                Ok(Response::Text { text })
            }
            Request::HealthGet => Ok(Response::Health(self.health().await)),
            Request::Subscribe { topics } => {
                *self.topics.lock().unwrap() = topics.into_iter().collect();
                Ok(Response::Ok)
            }
        }
    }

    async fn run_window(&self, id: i64) -> Result<Option<RunWindow>, RequestError> {
        window::run(&self.ctx.tmux, id).await.map_err(|e| rerr(ErrorCode::Internal, e))
    }

    /// Runs that still have a window, newest first, and their waiting approvals.
    fn run_page(&self, windows: &Windows) -> RunPage {
        let config = self.config();
        let mut details: Vec<RunDetail> = windows.runs.iter().map(|w| crate::runs::detail(&config, w)).collect();
        details.reverse();
        let waiting = details.iter().flat_map(|d| d.approvals.iter()).filter(|a| a.status == ApprovalStatus::Pending).cloned().collect();
        RunPage { runs: details.into_iter().map(|d| d.summary).collect(), waiting }
    }

    pub fn projects(&self, windows: &Windows) -> Vec<ProjectInfo> {
        let config = self.config();
        let git = self.git.lock().unwrap().clone();
        let runs: Vec<RunDetail> = windows.runs.iter().map(|w| crate::runs::detail(&config, w)).collect();
        config
            .projects
            .iter()
            .map(|p| ProjectInfo {
                id: p.id.clone(),
                path: p.path_display.clone(),
                remote: p.remote.clone(),
                disabled: p.disabled.clone(),
                busy_run: crate::runs::busy_run(windows, &p.id),
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
                services: p.services.iter().map(|s| crate::services::status(p, s, windows)).collect(),
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
                        last_run: runs
                            .iter()
                            .rev()
                            .find(|r| r.summary.project == p.id && r.summary.pipeline == pl.id)
                            .map(|r| r.summary.clone()),
                        waiting: runs
                            .iter()
                            .filter(|r| r.summary.project == p.id && r.summary.pipeline == pl.id)
                            .flat_map(|r| &r.approvals)
                            .filter(|a| a.status == ApprovalStatus::Pending)
                            .count() as u32,
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
        }
    }
}

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());

/// A capture without the blank rows below the last output.
pub fn trim_capture(pane: &str) -> String {
    let t = pane.trim_end_matches(['\n', ' ']);
    if t.is_empty() { String::new() } else { format!("{t}\n") }
}

/// Splits a run window's pane into each step's output, using the header the
/// worker prints before each step. The run's own status lines are dropped.
///
/// The scrollback is finite, so the oldest headers can be gone. Lines above
/// the first header still visible belong to the step before it; with no
/// header left at all, they belong to `latest`, the run's latest started step.
pub fn split_steps(pane: &str, latest: Option<usize>) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut current: Option<usize> = None;
    let mut orphans = String::new();
    let header = format!("── {}", crate::runs::worker::STEP_HEADER);
    for line in trim_capture(pane).lines() {
        let plain = ANSI_RE.replace_all(line, "");
        let plain = plain.trim();
        if let Some(rest) = plain.strip_prefix(&header)
            && let Some(n) = rest.split('/').next().and_then(|n| n.parse::<usize>().ok())
            && n > 0
        {
            if current.is_none() && n > 1 && !orphans.is_empty() {
                sections.resize(n - 1, String::new());
                sections[n - 2] = std::mem::take(&mut orphans);
            }
            current = Some(n - 1);
            if sections.len() < n {
                sections.resize(n, String::new());
            }
            continue;
        }
        if plain.starts_with("── run #") {
            continue;
        }
        // tmux resets the header's dim style at the start of the next line;
        // each step's view starts unstyled anyway.
        let line = line.strip_prefix("\x1b[0m").unwrap_or(line);
        let section = match current {
            Some(i) => &mut sections[i],
            None => &mut orphans,
        };
        section.push_str(line);
        section.push('\n');
    }
    if current.is_none()
        && !orphans.is_empty()
        && let Some(i) = latest
    {
        sections.resize(i + 1, String::new());
        sections[i] = orphans;
    }
    sections
}

/// The run's latest step that has started, which owns output whose header
/// has scrolled out of the pane.
pub fn latest_started(detail: &RunDetail) -> Option<usize> {
    detail.steps.iter().rev().find(|s| s.started_at.is_some()).map(|s| s.index)
}

fn step_chunk(detail: &RunDetail, sections: &[String], step: usize) -> LogChunk {
    let finished =
        detail.steps.get(step).is_none_or(|s| !matches!(s.status, StepStatus::Running | StepStatus::Pending | StepStatus::Waiting));
    LogChunk { run: detail.summary.id, step, data: sections.get(step).cloned().unwrap_or_default(), complete: finished }
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

/// What the poller last reported, to emit only changes. Values are JSON so
/// any protocol type can be compared.
#[derive(Default)]
struct Seen {
    primed: bool,
    runs: HashMap<i64, String>,
    steps: HashMap<(i64, usize), String>,
    approvals: HashMap<(i64, usize), String>,
    outputs: HashMap<(i64, String), String>,
    services: HashMap<(String, String), String>,
    logs: HashMap<(i64, usize), String>,
    service_logs: HashMap<(String, String), String>,
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

/// Turns what the windows publish into events, from one `list-windows` per
/// poll plus a pane capture for each log on screen.
async fn state_poller(e: Arc<Engine>) {
    let mut seen = Seen::default();
    loop {
        if let Ok(windows) = window::list(&e.ctx.tmux).await {
            let config = e.config();
            poll_runs(&e, &config, &windows, &mut seen);
            poll_services(&e, &config, &windows, &mut seen);
            poll_logs(&e, &config, &windows, &mut seen).await;
            seen.primed = true;
        }
        tokio::time::sleep(POLL).await;
    }
}

fn poll_runs(e: &Engine, config: &Config, windows: &Windows, seen: &mut Seen) {
    let details: Vec<RunDetail> = windows.runs.iter().map(|w| crate::runs::detail(config, w)).collect();
    let ids: HashSet<i64> = details.iter().map(|d| d.summary.id).collect();
    // Runs whose window closed are forgotten; the TUI refreshes its lists.
    let before = seen.runs.len();
    seen.runs.retain(|id, _| ids.contains(id));
    let mut any_change = seen.runs.len() != before;
    for d in &details {
        let id = d.summary.id;
        if changed(&mut seen.runs, id, &d.summary) {
            any_change = true;
            if seen.primed {
                e.emit(Event::Run(d.summary.clone()));
            }
        }
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
    seen.steps.retain(|(id, _), _| ids.contains(id));
    seen.approvals.retain(|(id, _), _| ids.contains(id));
    seen.outputs.retain(|(id, _), _| ids.contains(id));
    if any_change && seen.primed {
        e.emit(Event::Projects);
    }
}

fn poll_services(e: &Engine, config: &Config, windows: &Windows, seen: &mut Seen) {
    for p in &config.projects {
        for s in &p.services {
            let st = crate::services::status(p, s, windows);
            if changed(&mut seen.services, (p.id.clone(), s.id.clone()), &st) && seen.primed {
                e.emit(Event::Service(st));
            }
        }
    }
}

/// Re-captures the panes of the runs and services on screen and sends what changed.
async fn poll_logs(e: &Engine, config: &Config, windows: &Windows, seen: &mut Seen) {
    let topics: Vec<String> = e.topics.lock().unwrap().iter().cloned().collect();
    let runs: HashSet<i64> = topics.iter().filter_map(|t| t.strip_prefix("log:")?.parse().ok()).collect();
    seen.logs.retain(|(run, _), _| runs.contains(run));
    for run in runs {
        let Some(w) = windows.run(run) else { continue };
        let Ok(pane) = e.ctx.tmux.capture(run).await else { continue };
        let detail = crate::runs::detail(config, w);
        let sections = split_steps(&pane, latest_started(&detail));
        for step in 0..detail.steps.len().max(sections.len()) {
            let chunk = step_chunk(&detail, &sections, step);
            if changed(&mut seen.logs, (run, step), &(&chunk.data, chunk.complete)) {
                e.emit(Event::Log(chunk));
            }
        }
    }

    let services: Vec<(String, String)> = topics
        .iter()
        .filter_map(|t| t.strip_prefix("service-log:")?.split_once('/'))
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect();
    seen.service_logs.retain(|k, _| services.contains(k));
    for (pid, sid) in services {
        let Some(w) = crate::services::service_windows(windows, &pid, &sid).last().map(|w| w.id) else { continue };
        let Ok(pane) = e.ctx.tmux.capture(w).await else { continue };
        let data = trim_capture(&pane);
        if changed(&mut seen.service_logs, (pid.clone(), sid.clone()), &data) {
            e.emit(Event::ServiceLog { project: pid, service: sid, data });
        }
    }
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
    fn splits_a_run_pane_into_steps() {
        let pane = "\x1b[2m── run #4: app/hello ──\x1b[0m\n\x1b[2m── step 1/2: Greet ──\x1b[0m\n\x1b[0mhi\n\x1b[31mred\x1b[0m\n\
                    \x1b[2m── step 2/2: Use output ──\x1b[0m\ngot it\n\x1b[2m── run #4 succeeded ──\x1b[0m\n\n\n";
        let s = split_steps(pane, Some(1));
        assert_eq!(s, vec!["hi\n\x1b[31mred\x1b[0m\n".to_string(), "got it\n".to_string()]);
        assert_eq!(trim_capture("a\n\n  \n"), "a\n");
    }

    #[test]
    fn output_whose_header_scrolled_away_keeps_its_step() {
        // Step 2's header and the top of its output are gone from the scrollback.
        let pane = "…tail of step 2\n── step 3/3: Deploy ──\ndeployed\n";
        assert_eq!(split_steps(pane, Some(2)), vec![String::new(), "…tail of step 2\n".into(), "deployed\n".into()]);
        // No header left at all: everything is the latest started step's.
        assert_eq!(split_steps("line 9000\nline 9001\n", Some(1)), vec![String::new(), "line 9000\nline 9001\n".into()]);
        assert!(split_steps("line\n", None).is_empty());
        // Lines before step 1 are the run's own, not a step's.
        assert_eq!(split_steps("── run #3: a/b ──\nnote\n── step 1/1: X ──\nx\n", Some(0)), vec!["x\n".to_string()]);
    }
}
