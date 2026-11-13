//! End-to-end tests for services: real `groundcontrol svc-exec` workers in
//! windows of a private tmux server, driven through `services::control`.
//! Skipped when tmux is not installed.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::Env;
use groundcontrol::config::ServiceAction;
use groundcontrol::model::{ServiceState, ServiceStatus};
use groundcontrol::services;
use groundcontrol::tmux::target;

fn config(repo: &Path, port: u16) -> String {
    format!(
        r#"
projects:
  app:
    path: {repo}
    remote: github.com/me/app
    services:
      web:
        start: python3 -m http.server {port} --bind 127.0.0.1
        health: {{ url: "http://127.0.0.1:{port}/" }}
      worker:
        start: "sleep 1; exit 3"
        depends_on: [web]
      oneshot:
        start: "exit 4"
        restart: never
      secret:
        start: 'echo "token=$API_TOKEN"; sleep 30'
        env_file: secrets.env
      sleeper:
        start: sleep 30
    groups:
      stack: [web, worker]
"#,
        repo = repo.display()
    )
}

async fn control(env: &Env, target: &str, action: ServiceAction) -> Result<String, String> {
    services::control(&env.ctx, &env.config, "app", target, action).await
}

async fn status(env: &Env, id: &str) -> ServiceStatus {
    let p = env.config.project("app").unwrap();
    services::status(p, p.service(id).unwrap(), &env.windows().await)
}

async fn wait_status(env: &Env, id: &str, want: impl Fn(&ServiceStatus) -> bool) -> ServiceStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = status(env, id).await;
        if want(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "{id} did not reach the expected state: {s:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn window_of(env: &Env, id: &str) -> i64 {
    let w = env.windows().await;
    services::service_windows(&w, "app", id).last().expect("service window").id
}

/// The pid the service's worker published in its window.
async fn pid_of(env: &Env, id: &str) -> Option<i32> {
    let w = env.windows().await;
    services::service_windows(&w, "app", id).last().expect("service window").report.pid
}

#[tokio::test(flavor = "multi_thread")]
async fn services_end_to_end() {
    let Some(env) = common::setup(config) else { return };
    let port = env.port;

    // Port in use by something GroundControl did not start.
    let blocker = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let e = control(&env, "web", ServiceAction::Start).await.unwrap_err();
    assert!(e.contains(&format!("port {port} is in use (not started by GroundControl)")), "{e}");
    assert_eq!(status(&env, "web").await.state, ServiceState::Stopped);
    drop(blocker);

    // Start with an HTTP health check. The refused launch's dead window is replaced.
    control(&env, "web", ServiceAction::Start).await.unwrap();
    let web = status(&env, "web").await;
    assert_eq!(web.state, ServiceState::Running);
    assert!(web.healthy);
    assert!(pid_of(&env, "web").await.is_some());
    assert_eq!(services::service_windows(&env.windows().await, "app", "web").len(), 1);

    // Starting it again is a no-op.
    control(&env, "web", ServiceAction::Start).await.unwrap();
    assert_eq!(services::service_windows(&env.windows().await, "app", "web").len(), 1);

    // restart: never with a non-zero exit is reported as a failed start.
    let e = control(&env, "oneshot", ServiceAction::Start).await.unwrap_err();
    assert!(e.contains("exit code 4"), "{e}");
    assert_eq!(status(&env, "oneshot").await.state, ServiceState::Crashed);

    // on-failure restarts with back-off.
    control(&env, "worker", ServiceAction::Start).await.unwrap();
    let w = wait_status(&env, "worker", |s| s.restart_count >= 1).await;
    assert!(matches!(w.state, ServiceState::Restarting | ServiceState::Starting | ServiceState::Running), "{w:?}");

    // Group stop: everything stopped, windows closed, port free.
    control(&env, "stack", ServiceAction::Stop).await.unwrap();
    assert_eq!(status(&env, "web").await.state, ServiceState::Stopped);
    assert_eq!(status(&env, "worker").await.state, ServiceState::Stopped);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "web must be stopped");
    let left = env.windows().await;
    assert!(services::service_windows(&left, "app", "web").is_empty());
    assert!(services::service_windows(&left, "app", "worker").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn masking_and_stop_paths() {
    let Some(env) = common::setup(config) else { return };
    let t = &env.ctx.tmux;

    // env_file values are masked in the pane, the only copy of the output.
    control(&env, "secret", ServiceAction::Start).await.unwrap();
    let secret = window_of(&env, "secret").await;
    let mut pane = String::new();
    for _ in 0..50 {
        pane = t.capture(secret).await.unwrap();
        if pane.contains("token=") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(pane.contains("token=***"), "{pane}");
    assert!(!pane.contains("sup3r-s3cret-value"), "{pane}");

    // A stop works even while the window is scrolled back in copy mode,
    // where C-c would go to tmux instead of the worker.
    env.tmux(&["copy-mode", "-t", &target(secret)]);
    let started = Instant::now();
    control(&env, "secret", ServiceAction::Stop).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "stop took {:?}", started.elapsed());
    assert_eq!(status(&env, "secret").await.state, ServiceState::Stopped);

    // C-c in the window stops it by hand; the dead window stays readable.
    control(&env, "sleeper", ServiceAction::Start).await.unwrap();
    let sleeper = window_of(&env, "sleeper").await;
    t.send_keys(sleeper, &["C-c"]).await.unwrap();
    let s = wait_status(&env, "sleeper", |s| s.state == ServiceState::Stopped).await;
    assert_eq!(s.message, None);
    assert!(env.windows().await.services.iter().find(|w| w.id == sleeper).unwrap().dead);

    // A second worker for a running service gives up and closes its own window.
    control(&env, "sleeper", ServiceAction::Start).await.unwrap();
    let sleeper = window_of(&env, "sleeper").await;
    let pid = pid_of(&env, "sleeper").await.unwrap();
    let exe = env!("CARGO_BIN_EXE_groundcontrol");
    let cfg = format!("GC_CONFIG={}", env.cfg_path.display());
    let out = env.tmux(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-t",
        "=ground-control:",
        "-e",
        &cfg,
        "--",
        exe,
        "svc-exec",
        "app",
        "sleeper",
    ]);
    let dup: i64 = String::from_utf8(out.stdout).unwrap().trim().trim_start_matches('@').parse().unwrap();
    env.wait_for(10, "duplicate worker window closes itself", |w| !w.iter().any(|w| w.id == dup)).await;
    assert_eq!(status(&env, "sleeper").await.state, ServiceState::Running);

    // Closing the window (SIGHUP) still ends the service's process group.
    t.kill_window(sleeper).await.unwrap();
    env.wait_for(10, "window closed", |w| !w.iter().any(|w| w.id == sleeper)).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), None).is_ok() {
        assert!(Instant::now() < deadline, "the service's process group must be gone");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status(&env, "sleeper").await.state, ServiceState::Stopped);
}
