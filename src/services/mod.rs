//! Services without a daemon (docs/redesign-daemonless.md, "Services"): each
//! running service is a `groundcontrol svc-exec` worker in the tmux window
//! `gc-<project>:svc-<service>`. This module starts and stops those windows
//! in dependency order and reads their state back; `worker` is the window's
//! side.
//!
//! A service's row is written only by whoever holds its lock: the worker
//! while it runs, or a starter or stopper for the moment it takes the lock.

pub mod health;
pub mod policy;
pub mod worker;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::{Config, Health, Project, Service, ServiceAction};
pub use crate::ctx::Ctx;
use crate::lock;
use crate::protocol::{ServiceState, ServiceStatus, Timestamp, now};
use crate::store::StoredService;
use crate::tmux::NewWindow;

pub const HEALTH_START_TIMEOUT: Duration = Duration::from_secs(30);
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// How long a stop waits for the worker after asking, before closing its window.
const STOP_WAIT: Duration = Duration::from_secs(15);
/// A `waiting` row whose starter went away (the TUI quit mid-start) is stale after this.
const STALE_WAITING_SECS: Timestamp = 120;

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Service-specific helpers on the shared context.
impl Ctx {
    fn lock_path(&self, project: &str, service: &str) -> PathBuf {
        crate::paths::service_lock_path(&self.state_dir, project, service)
    }

    fn running(&self, project: &str, service: &str) -> bool {
        lock::is_held(&self.lock_path(project, service)).unwrap_or(false)
    }

    /// Runs `f` on the service's row while holding its lock. `Ok(false)` when
    /// a worker holds the lock, so the row is not ours to write.
    fn with_lock(&self, project: &str, service: &str, f: impl FnOnce(&mut StoredService)) -> Result<bool, String> {
        let Some(_lock) = lock::acquire(&self.lock_path(project, service)).map_err(|e| e.to_string())? else { return Ok(false) };
        let mut row = self.db(|s| s.service(project, service))?.unwrap_or_else(|| blank_row(project, service));
        f(&mut row);
        self.db(|s| s.save_service(&row))?;
        Ok(true)
    }

    /// Writes a launch's first row and clears any old stop request, under the
    /// lock. `Ok(false)` when a worker already holds the service.
    fn begin_launch(&self, row: &StoredService) -> Result<bool, String> {
        let Some(_lock) = lock::acquire(&self.lock_path(&row.project, &row.service)).map_err(|e| e.to_string())? else { return Ok(false) };
        self.db(|s| s.begin_service_launch(row))?;
        Ok(true)
    }
}

/// Window name for a service. Runs use `run-<id>`, so the prefixes keep the
/// two apart whatever the service is called.
pub fn window_name(service: &str) -> String {
    format!("svc-{service}")
}

pub(crate) fn blank_row(project: &str, service: &str) -> StoredService {
    StoredService {
        project: project.to_string(),
        service: service.to_string(),
        desired_running: false,
        pid: None,
        pgid: None,
        started_at: None,
        restart_count: 0,
        last_exit_code: None,
        status: ServiceState::Stopped,
        healthy: false,
        message: None,
        launch: None,
        updated_at: None,
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

/// A service's state from its row and whether a worker holds its lock.
pub fn status(ctx: &Ctx, project: &Project, svc: &Service) -> ServiceStatus {
    let mut st = ServiceStatus {
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
    let running = ctx.running(&project.id, &svc.id);
    let Ok(Some(row)) = ctx.db(|s| s.service(&project.id, &svc.id)) else { return st };
    st.state = row.status;
    st.healthy = row.healthy;
    st.pid = row.pid;
    st.started_at = row.started_at;
    st.restart_count = row.restart_count;
    st.last_exit_code = row.last_exit_code;
    st.message = row.message;
    if !running {
        match st.state {
            ServiceState::Starting
            | ServiceState::Running
            | ServiceState::Unhealthy
            | ServiceState::Restarting
            | ServiceState::Stopping => {
                st.state = ServiceState::Crashed;
                st.healthy = false;
                st.pid = None;
                st.message = Some("worker exited unexpectedly".into());
            }
            ServiceState::Waiting if row.updated_at.is_none_or(|t| now() - t > STALE_WAITING_SECS) => {
                st.state = ServiceState::Stopped;
                st.message = None;
            }
            _ => {}
        }
    }
    st
}

/// Starts, stops or restarts a service or a group. Returns a short summary.
pub async fn control(ctx: &Ctx, config: &Config, project_id: &str, target: &str, action: ServiceAction) -> Result<String, String> {
    let project = config.project(project_id).ok_or_else(|| format!("unknown project `{project_id}`"))?;
    let members: Vec<String> = if let Some(g) = project.group(target) {
        g.members.clone()
    } else if project.service(target).is_some() {
        vec![target.to_string()]
    } else {
        return Err(format!("unknown service or group `{target}` in {project_id}"));
    };
    let what = if project.group(target).is_some() { format!("group {target}") } else { target.to_string() };
    let order = start_order(project, &members);
    if matches!(action, ServiceAction::Stop | ServiceAction::Restart) {
        for id in order.iter().rev() {
            stop_one(ctx, project, id).await?;
        }
    }
    if matches!(action, ServiceAction::Start | ServiceAction::Restart) {
        if let Some(reason) = &project.disabled {
            return Err(format!("{project_id} is disabled: {reason}"));
        }
        for id in &order {
            start_one(ctx, project, id).await?;
        }
    }
    Ok(match action {
        ServiceAction::Start => format!("{what} started"),
        ServiceAction::Stop => format!("{what} stopped"),
        ServiceAction::Restart => format!("{what} restarted"),
    })
}

/// Starts one service after its dependencies are healthy. Returns once it is
/// healthy, or with the worker's reason when it did not start.
fn start_one<'a>(ctx: &'a Ctx, project: &'a Project, id: &'a str) -> BoxFuture<'a, Result<(), String>> {
    Box::pin(async move {
        let svc = project.service(id).ok_or_else(|| format!("unknown service `{id}`"))?;
        if ctx.running(&project.id, id) {
            return wait_ready(ctx, project, id, None).await.map_err(|e| format!("{id}: {e}"));
        }
        for dep in &svc.depends_on {
            ctx.with_lock(&project.id, id, |r| {
                r.status = ServiceState::Waiting;
                r.message = Some(format!("needs {dep}"));
            })?;
            if let Err(e) = start_one(ctx, project, dep).await {
                ctx.with_lock(&project.id, id, |r| {
                    r.status = ServiceState::Stopped;
                    r.message = Some(format!("needs {dep}, which failed to start"));
                })?;
                return Err(format!("{id}: dependency {dep} failed: {e}"));
            }
        }

        let token = format!("{}-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(), std::process::id());
        let row = StoredService {
            status: ServiceState::Waiting,
            message: Some("launching".into()),
            launch: Some(token.clone()),
            ..blank_row(&project.id, id)
        };
        if !ctx.begin_launch(&row)? {
            // Another starter got there first.
            return wait_ready(ctx, project, id, None).await.map_err(|e| format!("{id}: {e}"));
        }

        let name = window_name(id);
        match ctx.tmux.window(&project.id, &name).await? {
            // A live window whose worker has not taken the lock yet: another
            // starter's launch is in progress, so wait for that one.
            Some(w) if !w.dead => return wait_ready(ctx, project, id, None).await.map_err(|e| format!("{id}: {e}")),
            // A dead window from an earlier exit keeps the name taken.
            Some(_) => ctx.tmux.kill_window(&project.id, &name).await?,
            None => {}
        }
        let argv = [ctx.exe.to_string_lossy().into_owned(), "svc-exec".into(), project.id.clone(), id.to_string(), token.clone()];
        ctx.tmux
            .new_window(&NewWindow { project: &project.id, name: &name, cwd: &project.path, argv: &argv, env: &ctx.window_env() })
            .await?;
        wait_ready(ctx, project, id, Some(&token)).await.map_err(|e| format!("{id}: {e}"))
    })
}

/// Waits until the service is healthy. With `token`, only that launch's rows
/// count; without, whatever worker holds the service now.
async fn wait_ready(ctx: &Ctx, project: &Project, id: &str, token: Option<&str>) -> Result<(), String> {
    let deadline = Instant::now() + HEALTH_START_TIMEOUT + Duration::from_secs(15);
    let name = window_name(id);
    loop {
        // Lock first, then the row: a worker writes its final row before it
        // releases the lock, so a free lock means the row below is final.
        let running = ctx.running(&project.id, id);
        let row = ctx.db(|s| s.service(&project.id, id))?;
        let mine = row.as_ref().filter(|r| token.is_none_or(|t| r.launch.as_deref() == Some(t)));
        if let Some(r) = mine {
            match r.status {
                ServiceState::Running | ServiceState::Unhealthy => return Ok(()),
                ServiceState::Stopped | ServiceState::Crashed if token.is_some() || !running => {
                    return Err(r.message.clone().unwrap_or_else(|| "stopped".into()));
                }
                _ => {}
            }
        }
        if !running {
            match ctx.tmux.window(&project.id, &name).await? {
                Some(w) if !w.dead => {} // still starting up
                w => {
                    let status = w.and_then(|w| w.dead_status).map(|c| format!("exit code {c}")).unwrap_or_else(|| "no status".into());
                    return Err(format!("its worker exited ({status}) before the service started; see tmux window {name}"));
                }
            }
        }
        if Instant::now() >= deadline {
            return Err("did not become healthy in time".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stops one service: asks its worker, then closes the window if it doesn't
/// finish in time. The window is closed either way; the log file keeps the output.
async fn stop_one(ctx: &Ctx, project: &Project, id: &str) -> Result<(), String> {
    let name = window_name(id);
    if ctx.running(&project.id, id) {
        ctx.db(|s| s.request_service_stop(&project.id, id))?;
        if !wait_released(ctx, &project.id, id, STOP_WAIT).await {
            // SIGHUP: the worker still ends the child's process group.
            ctx.tmux.kill_window(&project.id, &name).await?;
            if !wait_released(ctx, &project.id, id, STOP_WAIT).await {
                return Err(format!("{id}: its worker did not stop; see tmux window {name}"));
            }
        }
    }
    ctx.tmux.kill_window(&project.id, &name).await?;
    // Covers a worker that died without writing its last row, and a crashed
    // service being acknowledged.
    ctx.with_lock(&project.id, id, |r| {
        r.status = ServiceState::Stopped;
        r.desired_running = false;
        r.healthy = false;
        r.pid = None;
        r.message = None;
    })?;
    Ok(())
}

async fn wait_released(ctx: &Ctx, project: &str, id: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while ctx.running(project, id) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
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
}
