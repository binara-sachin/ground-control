//! The daemon (`groundcontrol daemon`): owns every run and every service.

pub mod logfile;
pub mod process;
pub mod runner;
pub mod server;
pub mod store;
pub mod supervisor;
pub mod system;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::sync::{broadcast, mpsc};

use crate::config::{Config, Severity};
use crate::protocol::*;
use store::Store;
use supervisor::Supervisor;

const GIT_POLL: Duration = Duration::from_secs(60);
const SYSTEM_POLL: Duration = Duration::from_secs(60);
const TOOL_POLL: Duration = Duration::from_secs(30 * 60);
const STATE_SAVE: Duration = Duration::from_secs(30);
const RETENTION_DAYS: u64 = 90;

#[derive(Default)]
struct SystemState {
    tools: Vec<ToolStatus>,
    tools_checked_at: Option<Timestamp>,
    docker: Option<Reachability>,
    tmux: Option<Reachability>,
}

pub struct Daemon {
    pub started_at: Timestamp,
    pub config_path: PathBuf,
    config: RwLock<Arc<Config>>,
    config_status: Mutex<ConfigStatus>,
    store: Mutex<Store>,
    events: broadcast::Sender<Event>,
    pub supervisor: Supervisor,
    pub runs: Mutex<runner::Runs>,
    git: Mutex<HashMap<String, GitStatus>>,
    git_refresh: mpsc::UnboundedSender<Option<String>>,
    system: Mutex<SystemState>,
}

impl Daemon {
    pub fn new(config_path: PathBuf, store: Store) -> (Arc<Daemon>, mpsc::UnboundedReceiver<Option<String>>) {
        let (events, _) = broadcast::channel(4096);
        let (git_tx, git_rx) = mpsc::unbounded_channel();
        let d = Daemon {
            started_at: now(),
            config_status: Mutex::new(ConfigStatus {
                path: config_path.display().to_string(),
                ok: false,
                active: false,
                diagnostics: Vec::new(),
                last_attempt_at: None,
                loaded_at: None,
            }),
            config_path,
            config: RwLock::new(Arc::new(Config::default())),
            store: Mutex::new(store),
            events,
            supervisor: Supervisor::default(),
            runs: Mutex::new(runner::Runs::default()),
            git: Mutex::new(HashMap::new()),
            git_refresh: git_tx,
            system: Mutex::new(SystemState::default()),
        };
        (Arc::new(d), git_rx)
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap().clone()
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Runs a store operation, logging (not propagating) database errors.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> rusqlite::Result<T>) -> Option<T> {
        let store = self.store.lock().unwrap();
        match f(&store) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::error!("database error: {e}");
                None
            }
        }
    }

    pub fn request_git_refresh(&self, project: Option<String>) {
        let _ = self.git_refresh.send(project);
    }

    /// Loads the config file; keeps the previous good config when it is invalid.
    pub async fn reload_config(self: &Arc<Self>) {
        let path = self.config_path.clone();
        let result = tokio::task::spawn_blocking(move || crate::config::load_file(&path)).await;
        let Ok(result) = result else { return };
        let t = now();
        let ok = result.config.is_some();
        {
            let mut st = self.config_status.lock().unwrap();
            st.ok = ok;
            st.diagnostics = result.diagnostics.clone();
            st.last_attempt_at = Some(t);
            if let Some(c) = result.config {
                *self.config.write().unwrap() = Arc::new(c);
                st.active = true;
                st.loaded_at = Some(t);
            }
        }
        let errors = result.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        if ok {
            tracing::info!(warnings = result.diagnostics.len(), "config loaded");
            supervisor::sync_with_config(self).await;
            self.emit(Event::Projects);
            self.request_git_refresh(None);
        } else {
            tracing::warn!(errors, "config invalid; keeping the previous config");
        }
        self.emit(Event::Config(self.config_status()));
    }

    pub fn config_status(&self) -> ConfigStatus {
        self.config_status.lock().unwrap().clone()
    }

    pub fn projects(&self) -> Vec<ProjectInfo> {
        let config = self.config();
        let git = self.git.lock().unwrap().clone();
        let waiting = self.with_store(|db| db.waiting_approvals()).unwrap_or_default();
        let runs = self.runs.lock().unwrap();
        config
            .projects
            .iter()
            .map(|p| ProjectInfo {
                id: p.id.clone(),
                path: p.path_display.clone(),
                remote: p.remote.clone(),
                sensitive: p.sensitive,
                disabled: p.disabled.clone(),
                busy_run: runs.busy_run(&p.id).filter(|id| *id > 0),
                git: git.get(&p.id).cloned(),
                environments: p
                    .environments
                    .iter()
                    .map(|e| EnvInfo { name: e.name.clone(), protected: e.protected, var_names: e.vars.iter().map(|(k, _)| k.clone()).collect() })
                    .collect(),
                services: p.services.iter().map(|s| self.supervisor.status(p, s)).collect(),
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
                                    crate::config::StepKind::Run { run, .. } => run.clone(),
                                    crate::config::StepKind::Approve { message } => message.clone(),
                                    crate::config::StepKind::Service { action, target } => format!("{} {target}", action.as_str()),
                                },
                            })
                            .collect(),
                        warning: pl.warning.clone(),
                        last_run: self.with_store(|db| db.last_run(&p.id, &pl.id)).flatten(),
                        waiting: waiting.iter().filter(|a| a.project == p.id && a.pipeline == pl.id).count() as u32,
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn health(&self) -> HealthInfo {
        let sys = self.system.lock().unwrap();
        HealthInfo {
            tools: sys.tools.clone(),
            tools_checked_at: sys.tools_checked_at,
            docker: sys.docker.clone(),
            tmux: sys.tmux.clone(),
            config: self.config_status(),
            daemon_version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            started_at: self.started_at,
            pid: std::process::id(),
            state_dir: crate::paths::state_dir().display().to_string(),
        }
    }
}

fn init_logging() -> anyhow::Result<()> {
    let file = Arc::new(Mutex::new(logfile::RotatingFile::new(crate::paths::daemon_log_path())));
    let make = move || LogWriter(file.clone());
    tracing_subscriber::fmt().with_writer(make).with_ansi(false).with_target(false).init();
    Ok(())
}

struct LogWriter(Arc<Mutex<logfile::RotatingFile>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if std::env::var_os("GC_LOG_STDERR").is_some() {
            let _ = std::io::stderr().write_all(buf);
        }
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

fn prepare_state_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Entry point for `groundcontrol daemon`.
pub async fn run() -> anyhow::Result<()> {
    let state_dir = crate::paths::state_dir();
    prepare_state_dir(&state_dir)?;
    init_logging()?;
    let sock_path = crate::paths::socket_path();
    if sock_path.exists() {
        if tokio::net::UnixStream::connect(&sock_path).await.is_ok() {
            bail!("another GroundControl daemon is already listening on {}", sock_path.display());
        }
        std::fs::remove_file(&sock_path).ok();
    }
    let store = Store::open(&crate::paths::db_path()).context("opening state.db")?;
    let recovered = store.recover_interrupted(now())?;
    if !recovered.is_empty() {
        tracing::warn!(?recovered, "closed runs left active by the previous daemon");
    }
    let config_path = crate::paths::config_path();
    tracing::info!(version = VERSION, config = %config_path.display(), state = %state_dir.display(), "daemon starting");
    let (d, git_rx) = Daemon::new(config_path, store);
    d.reload_config().await;

    let listener = tokio::net::UnixListener::bind(&sock_path).with_context(|| format!("binding {}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    tokio::spawn(watch_config(d.clone()));
    tokio::spawn(git_poller(d.clone(), git_rx));
    tokio::spawn(system_poller(d.clone()));
    tokio::spawn(retention(d.clone()));
    supervisor::restore(&d).await;
    tokio::spawn(state_saver(d.clone()));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! {
        r = server::serve(d.clone(), listener) => { r?; }
        _ = term.recv() => tracing::info!("SIGTERM received"),
        _ = int.recv() => tracing::info!("SIGINT received"),
    }
    shutdown(&d).await;
    std::fs::remove_file(&sock_path).ok();
    Ok(())
}

async fn shutdown(d: &Arc<Daemon>) {
    tracing::info!("shutting down: cancelling runs and stopping services");
    let ids: Vec<i64> = d.runs.lock().unwrap().active_ids();
    for id in &ids {
        let _ = runner::cancel(d, *id);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while d.runs.lock().unwrap().active_count() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    supervisor::shutdown(d).await;
    d.with_store(|db| db.set_meta("last_saved_at", &now().to_string()));
}

async fn state_saver(d: Arc<Daemon>) {
    loop {
        d.with_store(|db| db.set_meta("last_saved_at", &now().to_string()));
        tokio::time::sleep(STATE_SAVE).await;
    }
}

/// Watches the config file's folder (editors replace files on save) and reloads.
async fn watch_config(d: Arc<Daemon>) {
    use notify::Watcher;
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let name = d.config_path.file_name().map(|n| n.to_os_string());
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if ev.paths.iter().any(|p| p.file_name().map(|n| n.to_os_string()) == name) {
                let _ = tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("cannot watch config: {e}");
            return;
        }
    };
    let dir = d.config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        tracing::error!("cannot watch {}: {e}", dir.display());
        return;
    }
    while rx.recv().await.is_some() {
        // Debounce bursts of events from a single save.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while rx.try_recv().is_ok() {}
        tracing::info!("config changed on disk; reloading");
        d.reload_config().await;
    }
}

async fn git_poller(d: Arc<Daemon>, mut rx: mpsc::UnboundedReceiver<Option<String>>) {
    let mut tick = tokio::time::interval(GIT_POLL);
    loop {
        let only = tokio::select! {
            _ = tick.tick() => None,
            r = rx.recv() => match r { Some(p) => p, None => return },
        };
        let config = d.config();
        for p in config.projects.iter().filter(|p| only.as_ref().is_none_or(|o| o == &p.id)) {
            if p.disabled.is_some() {
                d.git.lock().unwrap().remove(&p.id);
                continue;
            }
            let status = crate::git::status(&p.path, p.remote.as_deref()).await;
            let changed = {
                let mut cache = d.git.lock().unwrap();
                let changed = cache.get(&p.id).is_none_or(|old| GitStatus { checked_at: status.checked_at, ..old.clone() } != status);
                cache.insert(p.id.clone(), status.clone());
                changed
            };
            if changed {
                d.emit(Event::Git { project: p.id.clone(), git: status });
            }
        }
    }
}

async fn system_poller(d: Arc<Daemon>) {
    let mut last_tools: Option<tokio::time::Instant> = None;
    loop {
        if last_tools.is_none_or(|t| t.elapsed() >= TOOL_POLL) {
            let tools = system::tool_check().await;
            let missing: Vec<&str> = tools.iter().filter(|t| !t.ok).map(|t| t.name.as_str()).collect();
            if !missing.is_empty() {
                tracing::warn!(?missing, "tools not found on the step shell's PATH");
            }
            let mut s = d.system.lock().unwrap();
            s.tools = tools;
            s.tools_checked_at = Some(now());
            last_tools = Some(tokio::time::Instant::now());
        }
        let (docker, tmux) = tokio::join!(system::docker_reachable(), system::tmux_reachable());
        {
            let mut s = d.system.lock().unwrap();
            s.docker = Some(docker);
            s.tmux = Some(tmux);
        }
        tokio::time::sleep(SYSTEM_POLL).await;
    }
}

/// Prunes run logs and saved diffs older than 90 days (§11).
async fn retention(d: Arc<Daemon>) {
    loop {
        let mut roots = vec![crate::paths::state_dir()];
        roots.extend(d.config().projects.iter().filter(|p| p.sensitive).map(|p| p.data_root()));
        let removed = tokio::task::spawn_blocking(move || {
            let cutoff = std::time::SystemTime::now() - Duration::from_secs(RETENTION_DAYS * 24 * 3600);
            let mut n = 0;
            for root in roots {
                n += prune_older(&root.join("logs"), cutoff) + prune_older(&root.join("diffs"), cutoff);
                n += prune_older(&root.join("tmp"), std::time::SystemTime::now() - Duration::from_secs(7 * 24 * 3600));
            }
            n
        })
        .await
        .unwrap_or(0);
        if removed > 0 {
            tracing::info!(removed, "pruned old run logs and diffs");
        }
        tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    }
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
