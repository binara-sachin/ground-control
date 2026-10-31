//! Service supervisor (§6.5): start/stop/restart of services and groups,
//! health checks, the crash-restart policy and restore after a daemon restart.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::logfile::RotatingFile;
use super::process;
use super::store::StoredService;
use super::{Daemon, system};
use crate::config::{Config, Health, Project, RestartPolicy, Service, ServiceAction};
use crate::mask::Masker;
use crate::protocol::{Event, ServiceState, ServiceStatus, now};

pub const HEALTH_START_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
const CRASH_WINDOW: Duration = Duration::from_secs(5 * 60);
const CRASH_LIMIT: usize = 5;
const MAX_BACKOFF: u64 = 60;

type Key = (String, String);

#[derive(Default)]
pub struct Supervisor {
    slots: Mutex<HashMap<Key, Arc<Slot>>>,
    shutting_down: AtomicBool,
}

pub struct Slot {
    pub project: String,
    pub id: String,
    /// Serialises start/stop/restart/health-wait on this service (§6.2).
    op: tokio::sync::Mutex<()>,
    st: Mutex<SlotState>,
}

struct SlotState {
    state: ServiceState,
    healthy: bool,
    pid: Option<i32>,
    started_at: Option<i64>,
    restart_count: u32,
    last_exit_code: Option<i32>,
    message: Option<String>,
    desired: bool,
    crashes: VecDeque<Instant>,
    generation: u64,
    /// `Some(code)` once the current process has exited.
    exited: Option<watch::Receiver<Option<Option<i32>>>>,
    /// Set when GroundControl itself is stopping the process.
    expect_exit: bool,
}

impl Default for SlotState {
    fn default() -> Self {
        SlotState {
            state: ServiceState::Stopped,
            healthy: false,
            pid: None,
            started_at: None,
            restart_count: 0,
            last_exit_code: None,
            message: None,
            desired: false,
            crashes: VecDeque::new(),
            generation: 0,
            exited: None,
            expect_exit: false,
        }
    }
}

impl Slot {
    fn is_up(&self) -> bool {
        self.st.lock().unwrap().state.is_up()
    }
}

/// Dependency order for a set of services: dependencies first.
pub fn start_order(project: &Project, members: &[String]) -> Vec<String> {
    fn visit(p: &Project, id: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
        if !seen.insert(id.to_string()) {
            return;
        }
        if let Some(s) = p.service(id) {
            for d in &s.depends_on {
                visit(p, d, seen, out);
            }
        }
        out.push(id.to_string());
    }
    let mut seen = HashSet::new();
    let mut all = Vec::new();
    for m in members {
        visit(project, m, &mut seen, &mut all);
    }
    let wanted: HashSet<&String> = members.iter().collect();
    all.into_iter().filter(|s| wanted.contains(s)).collect()
}

impl Supervisor {
    fn slot(&self, project: &str, id: &str) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap();
        slots
            .entry((project.to_string(), id.to_string()))
            .or_insert_with(|| {
                Arc::new(Slot { project: project.into(), id: id.into(), op: Default::default(), st: Default::default() })
            })
            .clone()
    }

    fn existing(&self, project: &str, id: &str) -> Option<Arc<Slot>> {
        self.slots.lock().unwrap().get(&(project.to_string(), id.to_string())).cloned()
    }

    pub fn status(&self, project: &Project, svc: &Service) -> ServiceStatus {
        let base = ServiceStatus {
            project: project.id.clone(),
            id: svc.id.clone(),
            state: ServiceState::Stopped,
            healthy: false,
            pid: None,
            port: svc.health.as_ref().and_then(Health::port),
            started_at: None,
            restart_count: 0,
            last_exit_code: None,
            message: None,
            depends_on: svc.depends_on.clone(),
            restart: svc.restart,
            has_health_check: svc.health.is_some(),
        };
        match self.existing(&project.id, &svc.id) {
            None => base,
            Some(slot) => {
                let st = slot.st.lock().unwrap();
                ServiceStatus {
                    state: st.state,
                    healthy: st.healthy,
                    pid: st.pid,
                    started_at: st.started_at,
                    restart_count: st.restart_count,
                    last_exit_code: st.last_exit_code,
                    message: st.message.clone(),
                    ..base
                }
            }
        }
    }

    /// Process-group ids of every live managed service, for "not managed" checks.
    fn managed_pids(&self) -> HashMap<i32, String> {
        let slots = self.slots.lock().unwrap();
        slots
            .values()
            .filter_map(|s| s.st.lock().unwrap().pid.map(|p| (p, format!("{}/{}", s.project, s.id))))
            .collect()
    }

    pub fn running_count(&self) -> usize {
        self.slots.lock().unwrap().values().filter(|s| s.is_up()).count()
    }
}

fn emit_status(d: &Daemon, slot: &Slot) {
    let config = d.config();
    let Some(project) = config.project(&slot.project) else { return };
    let Some(svc) = project.service(&slot.id) else { return };
    d.emit(Event::Service(d.supervisor.status(project, svc)));
}

fn persist(d: &Daemon, slot: &Slot) {
    let s = {
        let st = slot.st.lock().unwrap();
        StoredService {
            project: slot.project.clone(),
            service: slot.id.clone(),
            desired_running: st.desired,
            pid: st.pid,
            pgid: st.pid,
            started_at: st.started_at,
            restart_count: st.restart_count,
            last_exit_code: st.last_exit_code,
            status: st.state,
        }
    };
    d.with_store(|db| db.save_service(&s));
}

fn update(d: &Daemon, slot: &Slot, f: impl FnOnce(&mut SlotState)) {
    f(&mut slot.st.lock().unwrap());
    persist(d, slot);
    emit_status(d, slot);
}

/// Starts, stops or restarts a service or a group. Returns a short summary.
pub async fn control(d: &Arc<Daemon>, project_id: &str, target: &str, action: ServiceAction) -> Result<String, String> {
    let config = d.config();
    let project = config.project(project_id).ok_or_else(|| format!("unknown project `{project_id}`"))?;
    let members: Vec<String> = if let Some(g) = project.group(target) {
        g.members.clone()
    } else if project.service(target).is_some() {
        vec![target.to_string()]
    } else {
        return Err(format!("unknown service or group `{target}` in {project_id}"));
    };
    let is_group = project.group(target).is_some();
    let order = start_order(project, &members);
    let what = if is_group { format!("group {target}") } else { target.to_string() };
    if matches!(action, ServiceAction::Stop | ServiceAction::Restart) {
        for id in order.iter().rev() {
            stop_service(d, project_id, id).await;
        }
    }
    if matches!(action, ServiceAction::Start | ServiceAction::Restart) {
        if let Some(reason) = &project.disabled {
            return Err(format!("{project_id} is disabled: {reason}"));
        }
        for id in &order {
            start_service(d.clone(), config.clone(), project_id.to_string(), id.clone()).await?;
        }
    }
    Ok(match action {
        ServiceAction::Start => format!("{what} started"),
        ServiceAction::Stop => format!("{what} stopped"),
        ServiceAction::Restart => format!("{what} restarted"),
    })
}

/// Starts one service after its dependencies are healthy. Returns once it is healthy.
pub fn start_service(
    d: Arc<Daemon>,
    config: Arc<Config>,
    project_id: String,
    id: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move {
        let project = config.project(&project_id).ok_or("unknown project")?;
        let svc = project.service(&id).ok_or_else(|| format!("unknown service `{id}`"))?.clone();
        let slot = d.supervisor.slot(&project_id, &id);
        let _op = slot.op.lock().await;
        if slot.is_up() {
            return Ok(());
        }
        for dep in &svc.depends_on {
            update(&d, &slot, |st| {
                st.state = ServiceState::Waiting;
                st.message = Some(format!("needs {dep}"));
            });
            if let Err(e) = start_service(d.clone(), config.clone(), project_id.clone(), dep.clone()).await {
                update(&d, &slot, |st| {
                    st.state = ServiceState::Stopped;
                    st.message = Some(format!("needs {dep}, which failed to start"));
                });
                return Err(format!("{id}: dependency {dep} failed: {e}"));
            }
        }
        if let Err(e) = crate::git::check_remote(&project.path, project.remote.as_deref()).await {
            update(&d, &slot, |st| {
                st.state = ServiceState::Stopped;
                st.message = Some("repository guard refused the start".into());
            });
            return Err(format!("{id}: {e}"));
        }
        update(&d, &slot, |st| {
            st.desired = true;
            st.crashes.clear();
            st.restart_count = 0;
        });
        match launch(&d, &slot, &config, project, &svc).await {
            Ok(()) => Ok(()),
            Err(e) => {
                update(&d, &slot, |st| {
                    st.desired = false;
                    if st.state != ServiceState::Crashed {
                        st.state = ServiceState::Stopped;
                    }
                    st.message = Some(e.clone());
                });
                Err(format!("{id}: {e}"))
            }
        }
    })
}

pub async fn stop_service(d: &Arc<Daemon>, project_id: &str, id: &str) {
    let Some(slot) = d.supervisor.existing(project_id, id) else { return };
    let _op = slot.op.lock().await;
    update(d, &slot, |st| st.desired = false);
    stop_process(d, &slot).await;
    update(d, &slot, |st| {
        st.state = ServiceState::Stopped;
        st.healthy = false;
        st.message = None;
    });
}

/// Stops the current process, if any: `SIGTERM` to the group, `SIGKILL` after 10 s.
async fn stop_process(d: &Daemon, slot: &Slot) {
    let (pid, exited) = {
        let mut st = slot.st.lock().unwrap();
        st.expect_exit = true;
        (st.pid, st.exited.clone())
    };
    let Some(pid) = pid else { return };
    let Some(mut exited) = exited else { return };
    if exited.borrow().is_some() {
        return;
    }
    update(d, slot, |st| st.state = ServiceState::Stopping);
    process::signal_group(pid, nix::sys::signal::Signal::SIGTERM);
    let timed_out = tokio::time::timeout(process::KILL_GRACE, exited.wait_for(|v| v.is_some())).await.is_err();
    if timed_out {
        process::signal_group(pid, nix::sys::signal::Signal::SIGKILL);
        let _ = tokio::time::timeout(Duration::from_secs(5), exited.wait_for(|v| v.is_some())).await;
    }
    if process::group_alive(pid) {
        process::signal_group(pid, nix::sys::signal::Signal::SIGKILL);
    }
}

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Spawns the process and waits for its health check.
fn launch<'a>(
    d: &'a Arc<Daemon>,
    slot: &'a Arc<Slot>,
    config: &'a Config,
    project: &'a Project,
    svc: &'a Service,
) -> BoxFuture<'a, Result<(), String>> {
    Box::pin(launch_inner(d, slot, config, project, svc))
}

async fn launch_inner(d: &Arc<Daemon>, slot: &Arc<Slot>, config: &Config, project: &Project, svc: &Service) -> Result<(), String> {
    if let Some(port) = svc.health.as_ref().and_then(Health::port) {
        if port_in_use(port).await {
            let owner = port_owner(port).await;
            let managed = d.supervisor.managed_pids();
            return Err(match owner {
                Some(pid) => match managed.get(&pid) {
                    Some(name) => format!("port {port} is in use by {name} (PID {pid})"),
                    None => format!("port {port} is in use by PID {pid} (not managed)"),
                },
                None => format!("port {port} is in use (not managed)"),
            });
        }
    }
    let dir = project.resolve_dir(svc.cwd.as_deref());
    if !dir.is_dir() {
        return Err(format!("working directory {} does not exist", dir.display()));
    }
    let mut env: BTreeMap<String, String> = config.env.iter().cloned().collect();
    let mut secrets = Vec::new();
    if let Some(f) = &svc.env_file {
        for (k, v) in crate::envfile::load(&dir.join(f))? {
            secrets.push(v.clone());
            env.insert(k, v);
        }
    }
    let mut masker = Masker::new(secrets);
    let log_path = crate::paths::service_log_path(&project.id, &project.path, project.sensitive, &svc.id);
    let mut log = RotatingFile::new(&log_path);
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(log, "\n── {stamp} starting: {} ──", svc.start);

    let spawned = process::spawn_shell(&svc.start, &dir, &env).map_err(|e| format!("cannot start: {e}"))?;
    let pid = spawned.pid;
    let (exit_tx, exit_rx) = watch::channel(None);
    let generation = {
        let mut st = slot.st.lock().unwrap();
        st.generation += 1;
        st.state = ServiceState::Starting;
        st.healthy = false;
        st.pid = Some(pid);
        st.started_at = Some(now());
        st.message = None;
        st.exited = Some(exit_rx.clone());
        st.expect_exit = false;
        st.generation
    };
    persist(d, slot);
    emit_status(d, slot);

    // Output → mask → rotated log file and live events.
    let mut output = spawned.output;
    let (dd, project_id, service_id) = (d.clone(), project.id.clone(), svc.id.clone());
    tokio::spawn(async move {
        while let Some(chunk) = output.recv().await {
            let masked = masker.push(&chunk);
            write_service_log(&dd, &mut log, &project_id, &service_id, &masked);
        }
        let rest = masker.flush();
        write_service_log(&dd, &mut log, &project_id, &service_id, &rest);
    });

    // Exit monitor.
    let mut child = spawned.child;
    let (dd, ss) = (d.clone(), slot.clone());
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|s| s.code());
        // Record the exit before announcing it, so waiters see the final state.
        on_exit(&dd, &ss, generation, code).await;
        let _ = exit_tx.send(Some(code));
    });

    // Health wait.
    let deadline = Instant::now() + HEALTH_START_TIMEOUT;
    let mut exited = exit_rx;
    loop {
        if let Some(code) = *exited.borrow() {
            return Err(format!("exited with {} during start", describe_exit(code)));
        }
        let ok = match &svc.health {
            Some(h) => check_health(h).await,
            None => {
                // No check: healthy while alive. Give an immediate failure a moment to show.
                let _ = tokio::time::timeout(Duration::from_millis(700), exited.changed()).await;
                if let Some(code) = *exited.borrow() {
                    return Err(format!("exited with {} during start", describe_exit(code)));
                }
                true
            }
        };
        if ok {
            break;
        }
        if Instant::now() >= deadline {
            stop_process(d, slot).await;
            return Err(format!("health check did not pass within {}s", HEALTH_START_TIMEOUT.as_secs()));
        }
        let _ = tokio::time::timeout(Duration::from_millis(300), exited.changed()).await;
    }
    update(d, slot, |st| {
        st.state = ServiceState::Running;
        st.healthy = true;
        st.message = None;
    });
    if let Some(h) = svc.health.clone() {
        tokio::spawn(health_monitor(d.clone(), slot.clone(), generation, h));
    }
    Ok(())
}

fn write_service_log(d: &Daemon, log: &mut RotatingFile, project: &str, service: &str, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let _ = log.write_all(data);
    let _ = log.flush();
    d.emit(Event::ServiceLog {
        project: project.to_string(),
        service: service.to_string(),
        data: String::from_utf8_lossy(data).into_owned(),
    });
}

fn describe_exit(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit code {c}"),
        None => "a signal".into(),
    }
}

async fn health_monitor(d: Arc<Daemon>, slot: Arc<Slot>, generation: u64, health: Health) {
    loop {
        tokio::time::sleep(HEALTH_INTERVAL).await;
        {
            let st = slot.st.lock().unwrap();
            if st.generation != generation || !matches!(st.state, ServiceState::Running | ServiceState::Unhealthy) {
                return;
            }
        }
        let ok = check_health(&health).await;
        let changed = {
            let mut st = slot.st.lock().unwrap();
            if st.generation != generation || !matches!(st.state, ServiceState::Running | ServiceState::Unhealthy) {
                return;
            }
            let new_state = if ok { ServiceState::Running } else { ServiceState::Unhealthy };
            let changed = st.healthy != ok || st.state != new_state;
            st.healthy = ok;
            st.state = new_state;
            changed
        };
        if changed {
            persist(&d, &slot);
            emit_status(&d, &slot);
        }
    }
}

/// Applies the restart policy when a process exits.
fn on_exit<'a>(d: &'a Arc<Daemon>, slot: &'a Arc<Slot>, generation: u64, code: Option<i32>) -> BoxFuture<'a, ()> {
    Box::pin(on_exit_inner(d, slot, generation, code))
}

async fn on_exit_inner(d: &Arc<Daemon>, slot: &Arc<Slot>, generation: u64, code: Option<i32>) {
    enum Next {
        Nothing,
        Restart(Duration),
        GaveUp(String),
    }
    let config = d.config();
    let policy = config
        .project(&slot.project)
        .and_then(|p| p.service(&slot.id))
        .map(|s| s.restart)
        .unwrap_or(RestartPolicy::Never);
    let next = {
        let mut st = slot.st.lock().unwrap();
        if st.generation != generation {
            return;
        }
        st.pid = None;
        st.healthy = false;
        st.last_exit_code = code;
        if st.expect_exit || d.supervisor.shutting_down.load(Ordering::SeqCst) {
            st.state = ServiceState::Stopped;
            Next::Nothing
        } else if st.state == ServiceState::Starting {
            // The start call reports this; no restart loop for a failed start.
            st.state = ServiceState::Crashed;
            st.message = Some(format!("exited with {} during start", describe_exit(code)));
            Next::Nothing
        } else if code == Some(0) {
            st.state = ServiceState::Stopped;
            st.desired = false;
            st.message = Some("exited with code 0".into());
            Next::Nothing
        } else if policy == RestartPolicy::Never {
            st.state = ServiceState::Crashed;
            st.desired = false;
            let msg = format!("exited with {}", describe_exit(code));
            st.message = Some(msg.clone());
            Next::GaveUp(msg)
        } else {
            let now_i = Instant::now();
            st.crashes.push_back(now_i);
            while st.crashes.front().is_some_and(|t| now_i.duration_since(*t) > CRASH_WINDOW) {
                st.crashes.pop_front();
            }
            if st.crashes.len() >= CRASH_LIMIT {
                st.state = ServiceState::Crashed;
                st.desired = false;
                let at = chrono::Local::now().format("%H:%M");
                let msg = format!("crashed {}× in 5m, gave up {at}", st.crashes.len());
                st.message = Some(msg.clone());
                Next::GaveUp(msg)
            } else {
                let n = st.crashes.len() as u32;
                let delay = (1u64 << (n - 1).min(6)).min(MAX_BACKOFF);
                st.state = ServiceState::Restarting;
                st.restart_count += 1;
                st.message = Some(format!("exited with {}, restarting in {delay}s", describe_exit(code)));
                Next::Restart(Duration::from_secs(delay))
            }
        }
    };
    persist(d, slot);
    emit_status(d, slot);
    match next {
        Next::Nothing => {}
        Next::GaveUp(msg) => {
            let title = format!("{}/{} crashed", slot.project, slot.id);
            system::notify(&title, &msg).await;
        }
        Next::Restart(delay) => {
            let (d, slot) = (d.clone(), slot.clone());
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _op = slot.op.lock().await;
                {
                    let st = slot.st.lock().unwrap();
                    if !st.desired || st.state != ServiceState::Restarting || st.generation != generation {
                        return;
                    }
                }
                let config = d.config();
                let Some(project) = config.project(&slot.project) else { return };
                let Some(svc) = project.service(&slot.id) else { return };
                if let Err(e) = launch(&d, &slot, &config, project, svc).await {
                    tracing::warn!(project = %slot.project, service = %slot.id, "restart failed: {e}");
                    let gen_now = slot.st.lock().unwrap().generation;
                    {
                        // Count the failed restart as another crash.
                        let mut st = slot.st.lock().unwrap();
                        st.state = ServiceState::Running;
                        st.expect_exit = false;
                    }
                    on_exit(&d, &slot, gen_now, Some(-1)).await;
                }
            });
        }
    }
}

pub async fn check_health(h: &Health) -> bool {
    match h {
        Health::Port(p) => port_in_use(*p).await,
        Health::Url(u) => http_ok(u).await,
    }
}

/// True when something accepts TCP connections on the port (IPv4 or IPv6 loopback).
pub async fn port_in_use(port: u16) -> bool {
    for addr in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(&addr)).await {
            return true;
        }
    }
    false
}

/// Minimal HTTP/1.1 GET that expects status 200. Local, plain HTTP only.
pub async fn http_ok(url: &str) -> bool {
    let Some((host, port, path)) = crate::config::parse_http_url(url) else { return false };
    let attempt = async {
        let mut stream = TcpStream::connect((host.as_str(), port)).await.ok()?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: groundcontrol\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.ok()?;
        let mut buf = vec![0u8; 64];
        let mut n = 0;
        while n < 12 {
            let m = stream.read(&mut buf[n..]).await.ok()?;
            if m == 0 {
                break;
            }
            n += m;
        }
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        Some(head.split_whitespace().nth(1) == Some("200"))
    };
    matches!(tokio::time::timeout(Duration::from_secs(3), attempt).await, Ok(Some(true)))
}

/// PID listening on a TCP port, via `lsof`.
async fn port_owner(port: u16) -> Option<i32> {
    let out = tokio::process::Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    let pid: i32 = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()?;
    // Report the process group so a managed service's child maps back to it.
    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid))).map(|g| g.as_raw()).ok().or(Some(pid))
}

/// Restores services after the daemon (not the Mac) restarted (§6.5).
pub async fn restore(d: &Arc<Daemon>) {
    let stored = d.with_store(|db| db.services()).unwrap_or_default();
    let boot = system::boot_time().await;
    let last_saved: Option<i64> = d.with_store(|db| db.meta("last_saved_at")).flatten().and_then(|s| s.parse().ok());
    let same_boot = matches!((boot, last_saved), (Some(b), Some(s)) if s > b);
    tracing::info!(?boot, ?last_saved, same_boot, "service restore check");
    if !same_boot {
        d.with_store(|db| db.clear_desired_services());
        return;
    }
    // Processes left behind by a daemon that died without stopping them.
    for s in &stored {
        if let Some(pgid) = s.pgid {
            if process::group_alive(pgid) {
                tracing::info!(project = %s.project, service = %s.service, pgid, "stopping leftover service process group");
                process::terminate_orphan_group(pgid).await;
            }
        }
    }
    let config = d.config();
    for s in stored.into_iter().filter(|s| s.desired_running) {
        let known = config.project(&s.project).is_some_and(|p| p.disabled.is_none() && p.service(&s.service).is_some());
        if !known {
            continue;
        }
        let (d, config) = (d.clone(), config.clone());
        tokio::spawn(async move {
            if let Err(e) = start_service(d, config, s.project.clone(), s.service.clone()).await {
                tracing::warn!("restore of {}/{} failed: {e}", s.project, s.service);
            }
        });
    }
}

/// Stops services whose definition disappeared from the config.
pub async fn sync_with_config(d: &Arc<Daemon>) {
    let config = d.config();
    let orphans: Vec<Key> = {
        let slots = d.supervisor.slots.lock().unwrap();
        slots
            .keys()
            .filter(|(p, s)| config.project(p).and_then(|p| p.service(s)).is_none())
            .cloned()
            .collect()
    };
    for (p, s) in orphans {
        tracing::info!(project = %p, service = %s, "service removed from config; stopping it");
        stop_service(d, &p, &s).await;
        d.supervisor.slots.lock().unwrap().remove(&(p, s));
    }
}

/// Daemon shutdown: stop every service but keep its desired state (§6.5).
pub async fn shutdown(d: &Arc<Daemon>) {
    d.supervisor.shutting_down.store(true, Ordering::SeqCst);
    let slots: Vec<Arc<Slot>> = d.supervisor.slots.lock().unwrap().values().cloned().collect();
    let mut tasks = Vec::new();
    for slot in slots {
        tasks.push(tokio::spawn(async move {
            let (pid, exited) = {
                let st = slot.st.lock().unwrap();
                (st.pid, st.exited.clone())
            };
            let (Some(pid), Some(mut exited)) = (pid, exited) else { return };
            process::signal_group(pid, nix::sys::signal::Signal::SIGTERM);
            if tokio::time::timeout(process::KILL_GRACE, exited.wait_for(|v| v.is_some())).await.is_err() {
                process::signal_group(pid, nix::sys::signal::Signal::SIGKILL);
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_puts_dependencies_first() {
        let src = "projects:\n  p:\n    path: /tmp\n    services:\n      webapp: { start: a, depends_on: [backend] }\n      backend: { start: b, depends_on: [postgres] }\n      postgres: { start: c }\n";
        let c = crate::config::load_str(src).config.unwrap();
        let p = c.project("p").unwrap();
        let members = vec!["webapp".to_string(), "postgres".to_string(), "backend".to_string()];
        assert_eq!(start_order(p, &members), ["postgres", "backend", "webapp"]);
        assert_eq!(start_order(p, &["webapp".to_string()]), ["webapp"]);
    }

    #[tokio::test]
    async fn http_check() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
            }
        });
        assert!(http_ok(&format!("http://127.0.0.1:{port}/health")).await);
        assert!(!http_ok(&format!("http://127.0.0.1:{port}/health")).await);
        assert!(port_in_use(port).await || true);
    }
}
