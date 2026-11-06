//! `groundcontrol svc-exec <project> <service> <launch>`: runs one service in
//! the foreground of its tmux window (docs/redesign-daemonless.md, "Services").
//!
//! The worker holds the service lock for its whole life and is the only
//! writer of the service's row while it does. It releases the lock last,
//! after the child's process group is gone and the final row is written, so a
//! new start never races the old process for its port.
//!
//! Stopping: the stop-request flag in the store (what the TUI uses), C-c in
//! the window, SIGTERM, or SIGHUP when the window is closed. All of them end
//! the child's process group and leave a `stopped` row.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use super::policy::{self, Next};
use super::{HEALTH_INTERVAL, HEALTH_START_TIMEOUT, blank_row, health};
use crate::config::{Config, Health, Project, Service};
use crate::logfile::RotatingFile;
use crate::mask::Masker;
use crate::process;
use crate::protocol::{ServiceState, now};
use crate::store::{Store, StoredService};

const TICK: Duration = Duration::from_millis(250);
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

/// Closes the pane this worker runs in, when it runs in tmux.
pub(crate) async fn close_own_pane() {
    if let Ok(pane_id) = std::env::var("TMUX_PANE") {
        let _ = crate::tmux::Tmux::default_server().kill_pane(&pane_id).await;
    }
}

struct Worker {
    store: Store,
    row: StoredService,
}

impl Worker {
    fn save(&mut self) {
        if let Err(e) = self.store.save_service(&self.row) {
            say(&format!("cannot record state: {e}"));
        }
    }

    fn set(&mut self, f: impl FnOnce(&mut StoredService)) {
        f(&mut self.row);
        self.save();
    }

    fn stop_requested(&self) -> bool {
        self.store.service_stop_requested(&self.row.project, &self.row.service).unwrap_or(false)
    }

    /// Ends the worker without a process: records why and returns exit code 1.
    fn refuse(&mut self, msg: &str) -> i32 {
        say(msg);
        self.set(|r| {
            r.status = ServiceState::Stopped;
            r.desired_running = false;
            r.healthy = false;
            r.pid = None;
            r.message = Some(msg.to_string());
        });
        1
    }
}

/// Entry point. Returns the process exit code.
pub async fn run(project_id: &str, service_id: &str, launch: &str) -> i32 {
    let state = crate::paths::state_dir();
    if let Err(e) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&state) {
        say(&format!("cannot create {}: {e}", state.display()));
        return 1;
    }
    let lock_path = crate::paths::service_lock_path(&state, project_id, service_id);
    let _lock = match crate::lock::acquire(&lock_path) {
        Ok(Some(l)) => l,
        Ok(None) => {
            say(&format!("{project_id}/{service_id} is already running in another window"));
            // Duplicate window names would make every later target ambiguous.
            close_own_pane().await;
            return 1;
        }
        Err(e) => {
            say(&format!("cannot take the service lock: {e}"));
            return 1;
        }
    };
    let store = match Store::open(&state.join(crate::paths::DB_FILE)) {
        Ok(s) => s,
        Err(e) => {
            say(&format!("cannot open the state database: {e}"));
            return 1;
        }
    };
    let mut row = blank_row(project_id, service_id);
    row.launch = Some(launch.to_string());
    row.desired_running = true;
    let mut w = Worker { store, row };
    let mut signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => return w.refuse(&format!("cannot install signal handlers: {e}")),
    };

    let Some(config) = crate::config::load_file(&crate::paths::config_path()).config else {
        return w.refuse("groundcontrol.yaml is invalid; see the Health screen");
    };
    let Some(project) = config.project(project_id) else { return w.refuse(&format!("unknown project `{project_id}`")) };
    let Some(svc) = project.service(service_id) else { return w.refuse(&format!("unknown service `{service_id}` in {project_id}")) };
    serve(&mut w, &mut signals, &config, project, svc).await
}

async fn serve(w: &mut Worker, signals: &mut Signals, config: &Config, project: &Project, svc: &Service) -> i32 {
    if let Some(reason) = &project.disabled {
        return w.refuse(&format!("{} is disabled: {reason}", project.id));
    }
    if let Err(e) = crate::git::check_remote(&project.path, project.remote.as_deref()).await {
        return w.refuse(&e);
    }
    if let Some(port) = svc.health.as_ref().and_then(Health::port)
        && health::port_in_use(port).await
    {
        return w.refuse(&format!("port {port} is in use (not started by GroundControl)"));
    }
    let dir = project.resolve_dir(svc.cwd.as_deref());
    if !dir.is_dir() {
        return w.refuse(&format!("working directory {} does not exist", dir.display()));
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
            Err(e) => return w.refuse(&e),
        }
    }
    let log_path = crate::paths::service_log_path(&project.id, &project.path, project.sensitive, &svc.id);

    let mut crashes = VecDeque::new();
    // Only the very first launch can "fail to start". After that, a restart
    // that dies early or never gets healthy is one more crash.
    let mut first = true;
    loop {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut log = RotatingFile::new(&log_path);
        let _ = writeln!(log, "\n── {stamp} starting: {} ──", svc.start);
        say(&format!("{stamp} starting: {}", svc.start));

        let spawned = match process::spawn_shell(&svc.start, &dir, &env) {
            Ok(s) => s,
            Err(e) => return w.refuse(&format!("cannot start: {e}")),
        };
        let pgid = spawned.pid;
        let spawned_at = Instant::now();
        w.set(|r| {
            r.status = ServiceState::Starting;
            r.healthy = false;
            r.pid = Some(pgid);
            r.pgid = Some(pgid);
            r.started_at = Some(now());
            r.message = None;
        });

        // Output → mask → pane and rotated log.
        let mut output = spawned.output;
        let mut masker = Masker::new(secrets.clone());
        let pump = tokio::spawn(async move {
            let mut write = |data: &[u8]| {
                if !data.is_empty() {
                    pane(data);
                    let _ = log.write_all(data);
                    let _ = log.flush();
                }
            };
            while let Some(chunk) = output.recv().await {
                write(&masker.push(&chunk));
            }
            write(&masker.flush());
        });

        let mut child = spawned.child;
        let (exit_tx, mut exited) = watch::channel(None::<Option<i32>>);
        tokio::spawn(async move {
            let code = child.wait().await.ok().and_then(|s| s.code());
            let _ = exit_tx.send(Some(code));
        });

        let mut during_start = true;
        let deadline = spawned_at + HEALTH_START_TIMEOUT;
        let mut next_check = Instant::now();
        let code = loop {
            let stop = tokio::select! {
                biased;
                _ = signals.recv() => true,
                r = exited.wait_for(|v| v.is_some()) => break r.map(|v| v.flatten()).unwrap_or(None),
                _ = tokio::time::sleep(TICK) => w.stop_requested(),
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
                        r.status = ServiceState::Running;
                        r.healthy = true;
                    });
                    say("healthy");
                } else if Instant::now() >= deadline {
                    let msg = format!("health check did not pass within {}s", HEALTH_START_TIMEOUT.as_secs());
                    let code = stop_group(pgid, &mut exited).await;
                    if first {
                        finish_output(pump).await;
                        return w.refuse(&msg);
                    }
                    say(&msg);
                    break code;
                }
            } else {
                next_check = Instant::now() + HEALTH_INTERVAL;
                let state = if ok { ServiceState::Running } else { ServiceState::Unhealthy };
                if w.row.healthy != ok || w.row.status != state {
                    w.set(|r| {
                        r.status = state;
                        r.healthy = ok;
                    });
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
        w.row.pid = None;
        w.row.healthy = false;
        w.row.last_exit_code = code;
        match next {
            Next::Stopped { message } => {
                say(&message);
                w.set(|r| {
                    r.status = ServiceState::Stopped;
                    r.desired_running = false;
                    r.message = Some(message);
                });
                return 0;
            }
            Next::Crashed { message, gave_up } => {
                say(&message);
                w.set(|r| {
                    r.status = ServiceState::Crashed;
                    r.desired_running = false;
                    r.message = Some(message);
                });
                if gave_up {
                    pane(b"\x07");
                    tell(&format!("{}/{} crashed: {}", w.row.project, w.row.service, w.row.message.as_deref().unwrap_or(""))).await;
                }
                return 1;
            }
            Next::Restart { delay, message } => {
                say(&message);
                w.set(|r| {
                    r.status = ServiceState::Restarting;
                    r.restart_count += 1;
                    r.message = Some(message);
                });
                let until = Instant::now() + delay;
                while Instant::now() < until {
                    let stop = tokio::select! {
                        _ = signals.recv() => true,
                        _ = tokio::time::sleep(TICK.min(until.saturating_duration_since(Instant::now()))) => w.stop_requested(),
                    };
                    if stop {
                        say("stopped");
                        w.set(|r| {
                            r.status = ServiceState::Stopped;
                            r.desired_running = false;
                            r.message = None;
                        });
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
    w.set(|r| r.status = ServiceState::Stopping);
    let code = stop_group(pgid, exited).await;
    finish_output(pump).await;
    w.set(|r| {
        r.status = ServiceState::Stopped;
        r.desired_running = false;
        r.healthy = false;
        r.pid = None;
        r.last_exit_code = code;
        r.message = None;
    });
    say("stopped");
    0
}

/// Waits briefly for the last output. A background process that inherited the
/// pipe can keep it open forever, so this gives up after 2 s.
async fn finish_output(pump: tokio::task::JoinHandle<()>) {
    let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;
}
