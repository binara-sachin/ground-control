//! End-to-end tests for the TUI's engine: requests, and the events it derives
//! from what the workers publish in tmux, against a private tmux server. Also
//! opens the real TUI in a tmux window and reads its first screen. Skipped
//! when tmux is not installed.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use groundcontrol::config::ServiceAction;
use groundcontrol::ctx::Ctx;
use groundcontrol::engine::Engine;
use groundcontrol::protocol::*;
use groundcontrol::tmux::Tmux;
use tokio::sync::mpsc::UnboundedReceiver;

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
        health: {{ port: {port} }}
    pipelines:
      hello:
        steps:
          - run: "echo one; sleep 1; echo two"
      gated:
        steps:
          - approve: "Go?"
          - run: echo went
"#,
        repo = repo.display()
    )
}

fn engine(env: &common::Env) -> (std::sync::Arc<Engine>, UnboundedReceiver<Event>) {
    let ctx = Ctx::new(Tmux::with_socket(&env.socket), env!("CARGO_BIN_EXE_groundcontrol").into(), env.cfg_path.clone()).unwrap();
    Engine::open(ctx)
}

async fn next_event(rx: &mut UnboundedReceiver<Event>, what: &str, want: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(e)) if want(&e) => return e,
            Ok(Some(_)) => {}
            _ => panic!("no {what} event"),
        }
    }
}

fn run_params(pipeline: &str) -> RunStartParams {
    RunStartParams { project: "app".into(), pipeline: pipeline.into(), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_end_to_end() {
    let Some(env) = common::setup(config) else { return };
    let (engine, mut rx) = engine(&env);

    let Response::Projects(p) = engine.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(p[0].pipelines.len(), 2);
    assert_eq!(p[0].services[0].state, ServiceState::Stopped);

    // A run started through the engine streams step events and its whole
    // step output, captured from the run's pane.
    let Response::RunStarted { id } = engine.request(Request::RunStart(run_params("hello"))).await.unwrap() else { panic!() };
    engine.request(Request::Subscribe { topics: vec!["runs".into(), format!("log:{id}")] }).await.unwrap();
    next_event(&mut rx, "step", |e| matches!(e, Event::Step { run_id, .. } if *run_id == id)).await;
    let (mut text, mut succeeded) = (String::new(), false);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(text.contains("two") && succeeded) {
        assert!(Instant::now() < deadline, "output so far: {text:?}, succeeded: {succeeded}");
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Event::Log(c))) if c.run == id => text = c.data,
            Ok(Some(Event::Run(s))) if s.id == id && s.status == RunStatus::Succeeded => succeeded = true,
            _ => {}
        }
    }
    assert!(text.starts_with("one\n"), "{text:?}");
    let Response::Log(c) = engine.request(Request::LogRead { run: id, step: 0 }).await.unwrap() else { panic!() };
    assert_eq!(c.data, "one\ntwo\n");
    assert!(c.complete);
    let Response::Runs(page) = engine.request(Request::RunsList).await.unwrap() else { panic!() };
    assert_eq!(page.runs[0].id, id, "newest first");

    // Approvals arrive as events and are answered through the engine.
    let mut started = None;
    for _ in 0..50 {
        // The previous run's window stays alive for a moment after its final state.
        match engine.request(Request::RunStart(run_params("gated"))).await {
            Ok(Response::RunStarted { id }) => {
                started = Some(id);
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let gated = started.expect("gated run started");
    next_event(&mut rx, "approval", |e| matches!(e, Event::Approval(a) if a.run_id == gated && a.status == ApprovalStatus::Pending)).await;
    let Response::Runs(page) = engine.request(Request::RunsList).await.unwrap() else { panic!() };
    assert_eq!(page.waiting.len(), 1);
    let Response::Projects(p) = engine.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(p[0].busy_run, Some(gated));
    assert_eq!(p[0].pipelines.iter().find(|pl| pl.id == "gated").unwrap().waiting, 1);
    engine.request(Request::RunDecide { id: gated, step: 0, decision: Decision::Approve, confirmed: false }).await.unwrap();
    next_event(&mut rx, "finished gated run", |e| matches!(e, Event::Run(s) if s.id == gated && s.status == RunStatus::Succeeded)).await;

    // Services: start through the engine, see the status event and the pane output.
    engine.request(Request::Subscribe { topics: vec!["services".into(), "service-log:app/web".into()] }).await.unwrap();
    let r = engine.request(Request::ServiceControl { project: "app".into(), id: "web".into(), action: ServiceAction::Start }).await;
    assert!(r.is_ok(), "{r:?}");
    next_event(&mut rx, "service", |e| matches!(e, Event::Service(s) if s.id == "web" && s.state == ServiceState::Running)).await;
    next_event(
        &mut rx,
        "service log",
        |e| matches!(e, Event::ServiceLog { service, data, .. } if service == "web" && data.contains("starting")),
    )
    .await;
    let Response::Text { text } = engine.request(Request::ServiceLog { project: "app".into(), id: "web".into() }).await.unwrap() else {
        panic!()
    };
    assert!(text.contains("starting: python3"), "{text}");
    engine.request(Request::ServiceControl { project: "app".into(), id: "web".into(), action: ServiceAction::Stop }).await.unwrap();

    // Health: tools resolved in-process, tmux reachable, Docker only on the Health screen.
    let Response::Health(h) = engine.request(Request::HealthGet).await.unwrap() else { panic!() };
    assert!(h.tools.iter().any(|t| t.name == "git" && t.ok));
    assert!(h.tmux.as_ref().is_some_and(|t| t.ok), "{:?}", h.tmux);
    assert!(h.docker.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reload_keeps_last_good() {
    let Some(env) = common::setup(config) else { return };
    let (engine, mut rx) = engine(&env);
    let good = std::fs::read_to_string(&env.cfg_path).unwrap();
    // Give the watcher a moment to start, then break the file.
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(&env.cfg_path, good.replace("          - approve: \"Go?\"\n", "          - nope: 1\n")).unwrap();
    let Event::Config(status) = next_event(&mut rx, "config", |e| matches!(e, Event::Config(c) if !c.ok)).await else { unreachable!() };
    assert!(status.active, "previous config stays active");
    assert!(status.diagnostics.iter().any(|d| d.line > 0), "{:?}", status.diagnostics);
    let Response::Projects(p) = engine.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(p[0].pipelines.len(), 2, "the old pipelines still work");
    let Response::Health(h) = engine.request(Request::HealthGet).await.unwrap() else { panic!() };
    assert!(!h.config.ok);
}

/// Opens the real TUI in a tmux window and checks its first screen.
#[tokio::test(flavor = "multi_thread")]
async fn tui_renders_dashboard() {
    let Some(env) = common::setup(config) else { return };
    let cfg = format!("GC_CONFIG={}", env.cfg_path.display());
    let out = env.tmux(&["new-session", "-d", "-s", "tui", "-x", "140", "-y", "40", "-e", &cfg, "--", env!("CARGO_BIN_EXE_groundcontrol")]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut screen = String::new();
    while !(screen.contains("app") && screen.contains("config ok")) {
        assert!(Instant::now() < deadline, "TUI screen:\n{screen}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        screen = String::from_utf8_lossy(&env.tmux(&["capture-pane", "-p", "-t", "=tui:"]).stdout).into_owned();
    }
    assert!(screen.contains("dashboard") && screen.contains("health"), "{screen}");
    assert!(!screen.contains("history"), "{screen}");
    env.tmux(&["send-keys", "-t", "=tui:", "q"]);
}
