//! `groundcontrol svc-exec <project> <service>`: runs one service in the
//! foreground of its tmux window (docs/architecture.md, "Services").
//!
//! The worker publishes the service's state as its window's `@gc_*` options,
//! in the tmux server's memory. Its window stays alive (not dead) until the
//! service's process group is gone and the final state is written, so a new
//! start never races the old process for its port. If an older live window
//! already runs the same service, the worker closes its own window instead.
//!
//! Stopping: the `@gc_stop` option (what the TUI sets), C-c in the window,
//! SIGTERM, or SIGHUP when the window is closed. All of them end the child's
//! process group and leave the state `stopped`.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use super::policy::{self, Next};
use super::{HEALTH_INTERVAL, HEALTH_START_TIMEOUT, health};
use crate::config::{Config, Health, Project, Service};
use crate::mask::Masker;
use crate::process;
use crate::protocol::{ServiceState, now};
use crate::tmux::Tmux;
use crate::window::{self, ServiceReport, ServiceWindow};

/// How often a starting service's health check is tried.
const START_CHECK: Duration = Duration::from_millis(300);
/// A service without a health check counts as healthy once it has stayed up this long.
const NO_CHECK_GRACE: Duration = Duration::from_millis(700);

/// Writes to the pane. Never fails: once the window is closed the terminal is
/// gone and writes return EIO, which must not stop the cleanup.
pub(crate) fn pane(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

pub(crate) fn say(msg: &str) {
    pane(format!("\x1b[2m── {msg} ──\x1b[0m\n").as_bytes());
}

pub(crate) struct Signals {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

impl Signals {
    pub(crate) fn install() -> std::io::Result<Signals> {
        Ok(Signals { int: signal(SignalKind::interrupt())?, term: signal(SignalKind::terminate())?, hup: signal(SignalKind::hangup())? })
    }

    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.int.recv() => {}
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
        }
    }
}

/// Shows a message to every attached tmux client (see `Tmux::tell_clients`).
pub(crate) async fn tell(msg: &str) {
    if std::env::var_os("TMUX").is_some() {
        let _ = crate::tmux::Tmux::default_server().tell_clients(msg).await;
    }
}

/// Waits for the TUI to write a request to the worker's window. One `tmux
/// wait-for` child stays blocked for as long as nothing happens, so an idle
/// worker starts no processes; the TUI signals the channel after each write.
pub(crate) struct Waker {
    tmux: Tmux,
    channel: String,
    waiting: Option<Wait>,
}

/// A blocked `tmux wait-for`.
type Wait = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

impl Waker {
    pub(crate) fn new(tmux: Tmux, window: i64) -> Waker {
        Waker { tmux, channel: crate::tmux::wake_channel(window), waiting: None }
    }

    /// Resolves when the window has been sent a request. Safe to drop
    /// unfinished (in a `select!`): the same wait carries on next time.
    pub(crate) async fn woken(&mut self) {
        let waiting = self.waiting.get_or_insert_with(|| {
            let (tmux, channel) = (self.tmux.clone(), self.channel.clone());
            Box::pin(async move { tmux.wait_for(&channel).await })
        });
        let result = waiting.await;
        self.waiting = None;
        if result.is_err() {
            // The tmux server is going away; don't spin.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Closes the pane this worker runs in, when it runs in tmux.
pub(crate) async fn close_own_pane() {
    if let Ok(pane_id) = std::env::var("TMUX_PANE") {
        let _ = crate::tmux::Tmux::default_server().kill_pane(&pane_id).await;
    }
}

struct Worker {
    tmux: Tmux,
    waker: Waker,
    /// `$TMUX_PANE`: options set on it land on the worker's window.
    pane: String,
    project: String,
    service: String,
    report: ServiceReport,
}

impl Worker {
    async fn save(&mut self) {
        if let Err(e) = self.tmux.set_options(&self.pane, &ServiceWindow::update(&self.report)).await {
            say(&format!("cannot publish state: {e}"));
        }
    }

    async fn set(&mut self, f: impl FnOnce(&mut ServiceReport)) {
        f(&mut self.report);
        self.save().await;
    }

    async fn stop_requested(&self) -> bool {
        window::stop_requested(&self.tmux, &self.pane).await.unwrap_or(false)
    }

    /// Ends the worker without a process: publishes why and returns exit code 1.
    async fn refuse(&mut self, msg: &str) -> i32 {
        say(msg);
        self.set(|r| {
            r.state = ServiceState::Stopped;
            r.healthy = false;
            r.pid = None;
            r.msg = Some(msg.to_string());
        })
        .await;
        1
    }
}

/// Entry point. Returns the process exit code.
pub async fn run(project_id: &str, service_id: &str) -> i32 {
    let Ok(pane) = std::env::var("TMUX_PANE") else {
        say("svc-exec runs inside a tmux window that GroundControl opens");
        return 1;
    };
    let tmux = Tmux::default_server();
    // An older live window already runs this service: leave it alone.
    let mine = match tmux.pane_window(&pane).await {
        Ok(id) => id,
        Err(e) => {
            say(&format!("cannot find this service's window: {e}"));
            return 1;
        }
    };
    let windows = window::list(&tmux).await.unwrap_or_default();
    if let Some(older) = super::live_window(&windows, project_id, service_id)
        && older.id < mine
    {
        say(&format!("{project_id}/{service_id} is already running in window @{}", older.id));
        close_own_pane().await;
        return 1;
    }
    let waker = Waker::new(tmux.clone(), mine);
    let mut w =
        Worker { tmux, waker, pane, project: project_id.to_string(), service: service_id.to_string(), report: ServiceReport::default() };
    let mut signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => return w.refuse(&format!("cannot install signal handlers: {e}")).await,
    };

    let Some(config) = crate::config::load_file(&crate::paths::config_path()).config else {
        return w.refuse("groundcontrol.yaml is invalid; see the Health screen").await;
    };
    let Some(project) = config.project(project_id) else { return w.refuse(&format!("unknown project `{project_id}`")).await };
    let Some(svc) = project.service(service_id) else {
        return w.refuse(&format!("unknown service `{service_id}` in {project_id}")).await;
    };
    serve(&mut w, &mut signals, &config, project, svc).await
}

async fn serve(w: &mut Worker, signals: &mut Signals, config: &Config, project: &Project, svc: &Service) -> i32 {
    if let Some(reason) = &project.disabled {
        return w.refuse(&format!("{} is disabled: {reason}", project.id)).await;
    }
    if let Err(e) = crate::git::check_remote(&project.path, project.remote.as_deref()).await {
        return w.refuse(&e).await;
    }
    if let Some(port) = svc.health.as_ref().and_then(Health::port)
        && health::port_in_use(port).await
    {
        return w.refuse(&format!("port {port} is in use (not started by GroundControl)")).await;
    }
    let dir = project.resolve_dir(svc.cwd.as_deref());
    if !dir.is_dir() {
        return w.refuse(&format!("working directory {} does not exist", dir.display())).await;
    }
    let mut env: BTreeMap<String, String> = config.env.iter().cloned().collect();
    let mut secrets = Vec::new();
    if let Some(f) = &svc.env_file {
        match crate::envfile::load(&dir.join(f)) {
            Ok(vars) => {
                for (k, v) in vars {
                    secrets.push(v.clone());
                    env.insert(k, v);
                }
            }
            Err(e) => return w.refuse(&e).await,
        }
    }

    let mut crashes = VecDeque::new();
    // Only the very first launch can "fail to start". After that, a restart
    // that dies early or never gets healthy is one more crash.
    let mut first = true;
    loop {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        say(&format!("{stamp} starting: {}", svc.start));

        let spawned = match process::spawn_shell(&svc.start, &dir, &env) {
            Ok(s) => s,
            Err(e) => return w.refuse(&format!("cannot start: {e}")).await,
        };
        let pgid = spawned.pid;
        let spawned_at = Instant::now();
        w.set(|r| {
            r.state = ServiceState::Starting;
            r.healthy = false;
            r.pid = Some(pgid);
            r.started = Some(now());
            r.msg = None;
        })
        .await;

        // Output → mask → pane. The pane's scrollback is the only copy.
        let mut output = spawned.output;
        let mut masker = Masker::new(secrets.clone());
        let pump = tokio::spawn(async move {
            while let Some(chunk) = output.recv().await {
                pane(&masker.push(&chunk));
            }
            pane(&masker.flush());
        });

        let mut child = spawned.child;
        let (exit_tx, mut exited) = watch::channel(None::<Option<i32>>);
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            let _ = exit_tx.send(Some(code));
        });

        let mut during_start = true;
        let deadline = spawned_at + HEALTH_START_TIMEOUT;
        // Without a health check, the first check is due once the grace has passed.
        let mut next_check = if svc.health.is_some() { Instant::now() } else { spawned_at + NO_CHECK_GRACE };
        let code = loop {
            let stop = tokio::select! {
                biased;
                _ = signals.recv() => true,
                r = exited.wait_for(|v| v.is_some()) => break r.map(|v| v.flatten()).unwrap_or(None),
                _ = w.waker.woken() => w.stop_requested().await,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_check)) => false,
            };
            if stop {
                return stop_service(w, pgid, &mut exited, pump).await;
            }
            if Instant::now() < next_check {
                continue;
            }
            let ok = match &svc.health {
                Some(h) => health::check_health(h).await,
                None => spawned_at.elapsed() >= NO_CHECK_GRACE,
            };
            if during_start {
                if ok {
                    during_start = false;
                    next_check = Instant::now() + HEALTH_INTERVAL;
                    w.set(|r| {
                        r.state = ServiceState::Running;
                        r.healthy = true;
                    })
                    .await;
                    say("healthy");
                } else if Instant::now() < deadline {
                    next_check = Instant::now() + START_CHECK;
                } else {
                    let msg = format!("health check did not pass within {}s", HEALTH_START_TIMEOUT.as_secs());
                    let code = stop_group(pgid, &mut exited).await;
                    if first {
                        finish_output(pump).await;
                        return w.refuse(&msg).await;
                    }
                    say(&msg);
                    break code;
                }
            } else {
                next_check = Instant::now() + HEALTH_INTERVAL;
                let state = if ok { ServiceState::Running } else { ServiceState::Unhealthy };
                if w.report.healthy != ok || w.report.state != state {
                    w.set(|r| {
                        r.state = state;
                        r.healthy = ok;
                    })
                    .await;
                }
            }
        };

        // The main process exited on its own. Anything it left in its group
        // must not hold the port across a restart.
        if process::group_alive(pgid) {
            process::signal_group(pgid, Signal::SIGKILL);
        }
        finish_output(pump).await;
        let next = policy::after_exit(svc.restart, during_start && first, code, &mut crashes, Instant::now());
        first = false;
        w.report.pid = None;
        w.report.healthy = false;
        w.report.exit = code;
        match next {
            Next::Stopped { message } => {
                say(&message);
                w.set(|r| {
                    r.state = ServiceState::Stopped;
                    r.msg = Some(message);
                })
                .await;
                return 0;
            }
            Next::Crashed { message, gave_up } => {
                say(&message);
                w.set(|r| {
                    r.state = ServiceState::Crashed;
                    r.msg = Some(message);
                })
                .await;
                if gave_up {
                    pane(b"\x07");
                    tell(&format!("{}/{} crashed: {}", w.project, w.service, w.report.msg.as_deref().unwrap_or(""))).await;
                }
                return 1;
            }
            Next::Restart { delay, message } => {
                say(&message);
                w.set(|r| {
                    r.state = ServiceState::Restarting;
                    r.restarts += 1;
                    r.msg = Some(message);
                })
                .await;
                let until = tokio::time::Instant::now() + delay;
                loop {
                    let stop = tokio::select! {
                        _ = signals.recv() => true,
                        _ = w.waker.woken() => w.stop_requested().await,
                        _ = tokio::time::sleep_until(until) => break,
                    };
                    if stop {
                        say("stopped");
                        w.set(|r| {
                            r.state = ServiceState::Stopped;
                            r.msg = None;
                        })
                        .await;
                        return 0;
                    }
                }
            }
        }
    }
}

/// `SIGTERM` to the group, `SIGKILL` after the grace period, then any member
/// that is still alive. Returns the main process's exit code.
async fn stop_group(pgid: i32, exited: &mut watch::Receiver<Option<Option<i32>>>) -> Option<i32> {
    process::signal_group(pgid, Signal::SIGTERM);
    if tokio::time::timeout(process::KILL_GRACE, exited.wait_for(|v| v.is_some())).await.is_err() {
        process::signal_group(pgid, Signal::SIGKILL);
        let _ = tokio::time::timeout(Duration::from_secs(5), exited.wait_for(|v| v.is_some())).await;
    }
    if process::group_alive(pgid) {
        process::signal_group(pgid, Signal::SIGKILL);
    }
    exited.borrow().flatten()
}

async fn stop_service(
    w: &mut Worker,
    pgid: i32,
    exited: &mut watch::Receiver<Option<Option<i32>>>,
    pump: tokio::task::JoinHandle<()>,
) -> i32 {
    say("stopping");
    w.set(|r| r.state = ServiceState::Stopping).await;
    let code = stop_group(pgid, exited).await;
    finish_output(pump).await;
    w.set(|r| {
        r.state = ServiceState::Stopped;
        r.healthy = false;
        r.pid = None;
        r.exit = code;
        r.msg = None;
    })
    .await;
    say("stopped");
    0
}

/// Waits briefly for the last output. A background process that inherited the
/// pipe can keep it open forever, so this gives up after 2 s.
async fn finish_output(pump: tokio::task::JoinHandle<()>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;
}
