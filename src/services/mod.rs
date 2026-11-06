//! Services (docs/architecture.md, "Services"). Each running service is a
//! `groundcontrol svc-exec` worker in a window of the `ground-control` tmux
//! session, named `<project>/<service>`. The worker publishes the service's
//! state as the window's `@gc_*` options; this module starts and stops those
//! windows in dependency order and reads their state back. `worker` is the
//! window's side.

pub mod health;
pub mod policy;
pub mod worker;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::config::{Config, Health, Project, Service, ServiceAction};
pub use crate::ctx::Ctx;
use crate::protocol::{ServiceState, ServiceStatus};
use crate::tmux::{NewWindow, Window};

/// `@gc_kind` of a service window.
pub const KIND: &str = "svc";
pub const HEALTH_START_TIMEOUT: Duration = Duration::from_secs(30);
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// How long a stop waits for the worker after asking, before closing its window.
const STOP_WAIT: Duration = Duration::from_secs(15);

type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The service's windows, oldest first.
pub fn service_windows<'a>(windows: &'a [Window], project: &str, service: &str) -> Vec<&'a Window> {
    let mut v: Vec<&Window> = windows
        .iter()
        .filter(|w| w.opt("kind") == Some(KIND) && w.opt("project") == Some(project) && w.opt("service") == Some(service))
        .collect();
    v.sort_by_key(|w| w.id);
    v
}

/// The window whose worker owns the service: the oldest live one.
pub fn live_window<'a>(windows: &'a [Window], project: &str, service: &str) -> Option<&'a Window> {
    service_windows(windows, project, service).into_iter().find(|w| !w.dead)
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

/// A service's state from its windows: the live one, else the newest dead one
/// (a crash keeps its window), else stopped.
pub fn status(project: &Project, svc: &Service, windows: &[Window]) -> ServiceStatus {
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
    let all = service_windows(windows, &project.id, &svc.id);
    let Some(w) = all.iter().find(|w| !w.dead).or(all.last()) else { return st };
    st.state = ServiceState::parse(w.opt("state").unwrap_or("starting"));
    st.healthy = w.opt("healthy") == Some("1");
    st.pid = w.opt("pid").and_then(|v| v.parse().ok());
    st.started_at = w.opt("started").and_then(|v| v.parse().ok());
    st.restart_count = w.opt("restarts").and_then(|v| v.parse().ok()).unwrap_or(0);
    st.last_exit_code = w.opt("exit").and_then(|v| v.parse().ok());
    st.message = w.opt("msg").map(str::to_string);
    if w.dead
        && matches!(
            st.state,
            ServiceState::Starting | ServiceState::Running | ServiceState::Unhealthy | ServiceState::Restarting | ServiceState::Stopping
        )
    {
        st.state = ServiceState::Crashed;
        st.healthy = false;
        st.pid = None;
        st.message = Some("worker exited unexpectedly".into());
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
        if let Some(w) = live_window(&ctx.tmux.windows().await?, &project.id, id) {
            return wait_ready(ctx, project, id, w.id).await.map_err(|e| format!("{id}: {e}"));
        }
        for dep in &svc.depends_on {
            if let Err(e) = start_one(ctx, project, dep).await {
                return Err(format!("{id}: dependency {dep} failed: {e}"));
            }
        }
        // A dead window from an earlier exit is replaced.
        for w in service_windows(&ctx.tmux.windows().await?, &project.id, id) {
            if w.dead {
                ctx.tmux.kill_window(w.id).await?;
            }
        }
        let argv = [ctx.exe.to_string_lossy().into_owned(), "svc-exec".into(), project.id.clone(), id.to_string()];
        let name = format!("{}/{id}", project.id);
        let options =
            [("kind", KIND.to_string()), ("project", project.id.clone()), ("service", id.to_string()), ("state", "starting".into())];
        let wid = ctx
            .tmux
            .new_window(&NewWindow { name: &name, cwd: &project.path, argv: &argv, env: &ctx.window_env(), options: &options })
            .await?;
        wait_ready(ctx, project, id, wid).await.map_err(|e| format!("{id}: {e}"))
    })
}

/// Waits until the service in window `wid` is healthy.
async fn wait_ready(ctx: &Ctx, project: &Project, id: &str, mut wid: i64) -> Result<(), String> {
    let deadline = Instant::now() + HEALTH_START_TIMEOUT + Duration::from_secs(15);
    loop {
        let windows = ctx.tmux.windows().await?;
        match windows.iter().find(|w| w.id == wid) {
            // A worker that found an older one closes its own window; follow the older one.
            None => match live_window(&windows, &project.id, id) {
                Some(w) => wid = w.id,
                None => return Err("its window closed before the service started".into()),
            },
            Some(w) => match ServiceState::parse(w.opt("state").unwrap_or("starting")) {
                ServiceState::Running | ServiceState::Unhealthy => return Ok(()),
                ServiceState::Stopped | ServiceState::Crashed => {
                    return Err(w.opt("msg").unwrap_or("stopped").to_string());
                }
                _ if w.dead => {
                    let status = w.dead_status.map(|c| format!("exit code {c}")).unwrap_or_else(|| "no status".into());
                    return Err(format!("its worker exited ({status}) before the service started; see window {}/{id}", project.id));
                }
                _ => {}
            },
        }
        if Instant::now() >= deadline {
            return Err("did not become healthy in time".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Stops one service: asks its worker, then closes the window if it doesn't
/// finish in time. The window is closed either way.
async fn stop_one(ctx: &Ctx, project: &Project, id: &str) -> Result<(), String> {
    let windows = ctx.tmux.windows().await?;
    for w in service_windows(&windows, &project.id, id) {
        if !w.dead {
            ctx.tmux.request(w.id, &[("stop", "1".into())]).await?;
            if !wait_exited(ctx, w.id, STOP_WAIT).await {
                // SIGHUP: the worker still ends the service's process group.
                ctx.tmux.kill_window(w.id).await?;
                if !wait_exited(ctx, w.id, STOP_WAIT).await {
                    return Err(format!("{id}: its worker did not stop; see window {}/{id}", project.id));
                }
            }
        }
        ctx.tmux.kill_window(w.id).await?;
    }
    Ok(())
}

/// Waits until window `wid` is dead or gone.
async fn wait_exited(ctx: &Ctx, wid: i64, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        match ctx.tmux.window(wid).await {
            Ok(None) => return true,
            Ok(Some(w)) if w.dead => return true,
            _ => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let src = "projects:\n  p:\n    path: /tmp\n    services:\n      webapp: { start: a, depends_on: [backend] }\n      backend: { start: b, depends_on: [postgres] }\n      postgres: { start: c }\n";
        crate::config::load_str(src).config.unwrap()
    }

    #[test]
    fn order_puts_dependencies_first() {
        let c = config();
        let p = c.project("p").unwrap();
        let members = vec!["webapp".to_string(), "postgres".to_string(), "backend".to_string()];
        assert_eq!(start_order(p, &members), ["postgres", "backend", "webapp"]);
        assert_eq!(start_order(p, &["webapp".to_string()]), ["webapp"]);
    }

    fn win(id: i64, dead: bool, service: &str, opts: &[(&str, &str)]) -> Window {
        let mut all = vec![("kind", KIND), ("project", "p"), ("service", service)];
        all.extend_from_slice(opts);
        Window { id, dead, dead_status: None, opts: all.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    #[test]
    fn status_from_windows() {
        let c = config();
        let p = c.project("p").unwrap();
        let s = p.service("backend").unwrap();
        assert_eq!(status(p, s, &[]).state, ServiceState::Stopped);

        let live = win(4, false, "backend", &[("state", "running"), ("healthy", "1"), ("pid", "77"), ("restarts", "2")]);
        let st = status(p, s, &[win(3, true, "backend", &[("state", "crashed")]), live]);
        assert_eq!((st.state, st.healthy, st.pid, st.restart_count), (ServiceState::Running, true, Some(77), 2));

        let st = status(p, s, &[win(5, true, "backend", &[("state", "crashed"), ("msg", "crashed 5× in 5m")])]);
        assert_eq!((st.state, st.message.as_deref()), (ServiceState::Crashed, Some("crashed 5× in 5m")));

        let st = status(p, s, &[win(6, true, "backend", &[("state", "running")])]);
        assert_eq!((st.state, st.message.as_deref()), (ServiceState::Crashed, Some("worker exited unexpectedly")));

        assert_eq!(status(p, s, &[win(7, false, "postgres", &[("state", "running")])]).state, ServiceState::Stopped);
    }
}
