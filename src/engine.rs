//! The TUI's in-process engine (docs/architecture.md, "The TUI"). It answers
//! the TUI's calls and turns what the tmux-hosted workers publish (their
//! windows' `@gc_*` options and pane output) into `Event`s by polling tmux.
//! It owns no runs and no services, and stores nothing: quitting the TUI
//! stops only the engine.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::{Config, Pipeline, Project, ServiceAction, StepKind};
use crate::ctx::Ctx;
use crate::envlayers::{self, RunScope};
use crate::git::GitStatus;
use crate::model::*;
use crate::runs::pane::{latest_started, split_steps};
use crate::tmux::trim_capture;
use crate::window::{self, Windows};

const POLL: Duration = Duration::from_secs(1);
const GIT_POLL: Duration = Duration::from_secs(30);
/// Docker is checked only while the Health screen is open, at most this often.
const DOCKER_EVERY: Duration = Duration::from_secs(30);

/// Tools the steps are expected to find on `PATH` (§6.6).
const REQUIRED_TOOLS: [&str; 8] = ["git", "gh", "pnpm", "npm", "cargo", "go", "docker", "tmux"];

pub struct Engine {
    pub ctx: Ctx,
    started_at: Timestamp,
    config: RwLock<Arc<Config>>,
    config_status: Mutex<ConfigStatus>,
    git: Mutex<HashMap<String, GitStatus>>,
    git_refresh: mpsc::UnboundedSender<Option<String>>,
    watch: Mutex<Watch>,
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
            watch: Mutex::new(Watch::default()),
            events,
            docker: Mutex::new(None),
        });
        e.reload_config_now();
        tokio::spawn(watch_config(e.clone()));
        tokio::spawn(git_poller(e.clone(), git_rx));
        tokio::spawn(state_poller(e.clone()));
        (e, rx)
    }

    fn config(&self) -> Arc<Config> {
        self.config.read().unwrap().clone()
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
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

    fn config_status(&self) -> ConfigStatus {
        self.config_status.lock().unwrap().clone()
    }

    /// Replaces what the pollers follow; see `Watch`.
    pub fn watch(&self, watch: Watch) {
        *self.watch.lock().unwrap() = watch;
    }

    pub async fn projects(&self) -> Result<Vec<ProjectInfo>, String> {
        let windows = window::list(&self.ctx.tmux).await?;
        Ok(self.project_infos(&windows))
    }

    /// Opens a run's window, or only plans it when `params.dry_run` is set.
    pub async fn start_run(&self, params: RunStartParams) -> Result<StartOutcome, String> {
        let project = params.project.clone();
        let r = crate::runs::start(&self.ctx, &self.config(), params).await.map_err(|(_, m)| m);
        if r.is_ok() {
            self.emit(Event::Projects);
            let _ = self.git_refresh.send(Some(project));
        }
        r
    }

    pub async fn cancel_run(&self, id: i64) -> Result<(), String> {
        crate::runs::cancel(&self.ctx, id).await.map_err(|(_, m)| m)
    }

    pub async fn decide(&self, id: i64, step: usize, decision: Decision, confirmed: bool) -> Result<(), String> {
        crate::runs::decide(&self.ctx, id, step, decision, confirmed).await.map_err(|(_, m)| m)
    }

    pub async fn runs(&self) -> Result<RunList, String> {
        let windows = window::list(&self.ctx.tmux).await?;
        Ok(self.run_list(&windows))
    }

    pub async fn run(&self, id: i64) -> Result<RunDetail, String> {
        match window::run(&self.ctx.tmux, id).await? {
            Some(w) => Ok(crate::runs::detail(&self.config(), &w)),
            None => Err(format!("run #{id} has no window any more")),
        }
    }

    pub async fn step_output(&self, run: i64, step: usize) -> Result<LogChunk, String> {
        let Some(w) = window::run(&self.ctx.tmux, run).await? else {
            return Err(format!("run #{run} has no window any more"));
        };
        let pane = self.ctx.tmux.capture(run).await?;
        let detail = crate::runs::detail(&self.config(), &w);
        Ok(step_chunk(&detail, &split_steps(&pane, latest_started(&detail)), step))
    }

    pub async fn control_service(&self, project: &str, id: &str, action: ServiceAction) -> Result<String, String> {
        crate::services::control(&self.ctx, &self.config(), project, id, action).await
    }

    pub async fn service_output(&self, project: &str, id: &str) -> Result<String, String> {
        let windows = window::list(&self.ctx.tmux).await?;
        Ok(match crate::services::service_windows(&windows, project, id).last() {
            Some(w) => trim_capture(&self.ctx.tmux.capture(w.id).await?),
            None => String::new(),
        })
    }

    /// Runs that still have a window, newest first, and their waiting approvals.
    fn run_list(&self, windows: &Windows) -> RunList {
        let config = self.config();
        let mut details: Vec<RunDetail> = windows.runs.iter().map(|w| crate::runs::detail(&config, w)).collect();
        details.reverse();
        let waiting = details.iter().flat_map(|d| d.approvals.iter()).filter(|a| a.status == ApprovalStatus::Pending).cloned().collect();
        RunList { runs: details.into_iter().map(|d| d.summary).collect(), waiting }
    }

    fn project_infos(&self, windows: &Windows) -> Vec<ProjectInfo> {
        let config = self.config();
        let git = self.git.lock().unwrap().clone();
        let runs: Vec<RunDetail> = windows.runs.iter().map(|w| crate::runs::detail(&config, w)).collect();
        config
            .projects
            .iter()
            .map(|p| ProjectInfo {
                id: p.id.clone(),
                path: p.path_display.clone(),
                disabled: p.disabled.clone(),
                busy_run: crate::runs::busy_run(windows, &p.id),
                git: git.get(&p.id).cloned(),
                environments: p.environments.iter().map(|e| EnvInfo { name: e.name.clone(), protected: e.protected }).collect(),
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
                                previews: match &s.kind {
                                    StepKind::Approve { message } => approval_previews(&config, p, pl, message),
                                    _ => BTreeMap::new(),
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
        let docker = if self.watch.lock().unwrap().health {
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

/// An approval step's message as a dry run on each of the pipeline's
/// environments would show it, before inputs are chosen.
fn approval_previews(config: &Config, project: &Project, pipeline: &Pipeline, message: &str) -> BTreeMap<String, String> {
    let no_inputs = BTreeMap::new();
    let envs: Vec<Option<&str>> =
        if pipeline.environments.is_empty() { vec![None] } else { pipeline.environments.iter().map(|e| Some(e.as_str())).collect() };
    envs.into_iter()
        .map(|env| {
            let text = envlayers::approval_message(config, project, &RunScope::preview(env, &no_inputs), message);
            (env.unwrap_or("").to_string(), text)
        })
        .collect()
}

fn step_chunk(detail: &RunDetail, sections: &[String], step: usize) -> LogChunk {
    let finished =
        detail.steps.get(step).is_none_or(|s| !matches!(s.status, StepStatus::Running | StepStatus::Pending | StepStatus::Waiting));
    LogChunk { run: detail.summary.id, step, data: sections.get(step).cloned().unwrap_or_default(), complete: finished }
}

/// Looks a command up on `PATH` without running anything.
fn find_on_path(name: &str) -> Option<PathBuf> {
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

/// What the poller last reported, to emit only changes.
#[derive(Default)]
struct Seen {
    primed: bool,
    runs: HashMap<i64, RunSummary>,
    steps: HashMap<(i64, usize), StepInfo>,
    approvals: HashMap<(i64, usize), ApprovalInfo>,
    outputs: HashMap<(i64, String), String>,
    services: HashMap<(String, String), ServiceStatus>,
    logs: HashMap<(i64, usize), (String, bool)>,
    service_logs: HashMap<(String, String), String>,
}

fn changed<K: std::hash::Hash + Eq, V: PartialEq + Clone>(map: &mut HashMap<K, V>, key: K, value: &V) -> bool {
    match map.get(&key) {
        Some(old) if old == value => false,
        _ => {
            map.insert(key, value.clone());
            true
        }
    }
}

/// Turns what the windows publish into events, from one `list-windows` per
/// poll plus a pane capture for each run step or service output being followed.
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
    let Watch { runs, services, .. } = e.watch.lock().unwrap().clone();
    seen.logs.retain(|(run, _), _| runs.contains(run));
    for run in runs {
        let Some(w) = windows.run(run) else { continue };
        let Ok(pane) = e.ctx.tmux.capture(run).await else { continue };
        let detail = crate::runs::detail(config, w);
        let sections = split_steps(&pane, latest_started(&detail));
        for step in 0..detail.steps.len().max(sections.len()) {
            let chunk = step_chunk(&detail, &sections, step);
            if changed(&mut seen.logs, (run, step), &(chunk.data.clone(), chunk.complete)) {
                e.emit(Event::Log(chunk));
            }
        }
    }

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
}
