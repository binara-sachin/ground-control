//! End-to-end tests for the TUI's engine: requests and the events it derives
//! from what workers write, against a private tmux server. Also opens the
//! real TUI in a tmux window and reads its first screen. Skipped when tmux is
//! not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use groundcontrol::config::ServiceAction;
use groundcontrol::ctx::Ctx;
use groundcontrol::engine::Engine;
use groundcontrol::protocol::*;
use groundcontrol::tmux::Tmux;
use tokio::sync::mpsc::UnboundedReceiver;

struct Env {
    socket: PathBuf,
    base: PathBuf,
    cfg: PathBuf,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux").arg("-S").arg(&self.socket).arg("kill-server").output();
    }
}

fn sh(dir: &Path, cmd: &str) {
    let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap().success();
    assert!(ok, "{cmd}");
}

fn setup() -> Option<Env> {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return None;
    }
    let dir = tempfile::Builder::new().prefix("gce").tempdir_in(std::env::temp_dir()).unwrap();
    let base = dir.path().to_path_buf();
    let repo = base.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    sh(
        &repo,
        "git init -q -b main && git config user.email t@t && git config user.name t && \
         git remote add origin git@github.com:me/app.git && echo hi > README && git add -A && git commit -qm init",
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = base.join("groundcontrol.yaml");
    std::fs::write(
        &cfg,
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
        ),
    )
    .unwrap();
    Some(Env { socket: base.join("s"), cfg, port, base, _dir: dir })
}

impl Env {
    fn engine(&self) -> (std::sync::Arc<Engine>, UnboundedReceiver<Event>) {
        let ctx = Ctx::new(
            Tmux::with_socket(&self.socket),
            env!("CARGO_BIN_EXE_groundcontrol").into(),
            self.base.join("state"),
            self.cfg.clone(),
        )
        .unwrap();
        Engine::open(ctx)
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn engine_end_to_end() {
    let Some(env) = setup() else { return };
    let (engine, mut rx) = env.engine();

    let Response::Projects(p) = engine.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(p[0].pipelines.len(), 2);
    assert_eq!(p[0].services[0].state, ServiceState::Stopped);

    // A run started through the engine streams step and log events.
    engine.request(Request::Subscribe { topics: vec!["runs".into(), "log:1".into()] }).await.unwrap();
    let start = RunStartParams {
        project: "app".into(),
        pipeline: "hello".into(),
        env: None,
        inputs: Default::default(),
        dry_run: false,
        confirmed: false,
    };
    let Response::RunStarted { id } = engine.request(Request::RunStart(start)).await.unwrap() else { panic!() };
    assert_eq!(id, 1);
    next_event(&mut rx, "step", |e| matches!(e, Event::Step { run_id: 1, .. })).await;
    // The final status can arrive in the same poll as the last log chunk.
    let (mut text, mut succeeded) = (String::new(), false);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(text.contains("two") && succeeded) {
        assert!(Instant::now() < deadline, "log so far: {text:?}, succeeded: {succeeded}");
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Event::Log(c))) => text.push_str(&c.data),
            Ok(Some(Event::Run(s))) if s.id == 1 && s.status == RunStatus::Succeeded => succeeded = true,
            _ => {}
        }
    }
    let Response::Log(c) = engine.request(Request::LogRead { run: 1, step: 0, from_offset: 0 }).await.unwrap() else { panic!() };
    assert_eq!(c.data, "one\ntwo\n");

    // Approvals arrive as events and are answered through the engine.
    let gated = RunStartParams { pipeline: "gated".into(), ..RunStartParams { project: "app".into(), ..Default::default() } };
    let mut started = None;
    for _ in 0..50 {
        // The previous run's lock is released just after its final status.
        match engine.request(Request::RunStart(gated.clone())).await {
            Ok(Response::RunStarted { id }) => {
                started = Some(id);
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let id = started.expect("gated run started");
    next_event(&mut rx, "approval", |e| matches!(e, Event::Approval(a) if a.run_id == id && a.status == ApprovalStatus::Pending)).await;
    let Response::Runs(page) = engine.request(Request::RunsQuery { filter: RunFilter::default(), page: 0 }).await.unwrap() else {
        panic!()
    };
    assert_eq!(page.waiting.len(), 1);
    let Response::Projects(p) = engine.request(Request::ProjectsList).await.unwrap() else { panic!() };
    assert_eq!(p[0].busy_run, Some(id));
    engine.request(Request::RunDecide { id, step: 0, decision: Decision::Approve, confirmed: false }).await.unwrap();
    next_event(&mut rx, "finished gated run", |e| matches!(e, Event::Run(s) if s.id == id && s.status == RunStatus::Succeeded)).await;

    // Services: start through the engine, see the status event and the log.
    engine.request(Request::Subscribe { topics: vec!["services".into(), "service-log:app/web".into()] }).await.unwrap();
    let r = engine.request(Request::ServiceControl { project: "app".into(), id: "web".into(), action: ServiceAction::Start }).await;
    assert!(r.is_ok(), "{r:?}");
    next_event(&mut rx, "service", |e| matches!(e, Event::Service(s) if s.id == "web" && s.state == ServiceState::Running)).await;
    let _ = std::net::TcpStream::connect(("127.0.0.1", env.port)).map(|mut s| {
        use std::io::Write;
        let _ = s.write_all(b"GET / HTTP/1.0\r\n\r\n");
    });
    next_event(&mut rx, "service log", |e| matches!(e, Event::ServiceLog { service, .. } if service == "web")).await;
    engine.request(Request::ServiceControl { project: "app".into(), id: "web".into(), action: ServiceAction::Stop }).await.unwrap();

    // Health: tools resolved in-process, tmux reachable, Docker only on the Health screen.
    let Response::Health(h) = engine.request(Request::HealthGet).await.unwrap() else { panic!() };
    assert!(h.tools.iter().any(|t| t.name == "git" && t.ok));
    assert!(h.tmux.as_ref().is_some_and(|t| t.ok), "{:?}", h.tmux);
    assert!(h.docker.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reload_keeps_last_good() {
    let Some(env) = setup() else { return };
    let (engine, mut rx) = env.engine();
    let good = std::fs::read_to_string(&env.cfg).unwrap();
    // Give the watcher a moment to start, then break the file.
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::write(
        &env.cfg,
        good.replace("      gated:\n        steps:\n          - approve: \"Go?\"\n", "      gated:\n        steps:\n          - nope: 1\n"),
    )
    .unwrap();
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
    let Some(env) = setup() else { return };
    let tmux = |args: &[&str]| Command::new("tmux").arg("-S").arg(&env.socket).args(args).output().unwrap();
    let state = env.base.join("state");
    let out = tmux(&[
        "new-session",
        "-d",
        "-s",
        "tui",
        "-x",
        "140",
        "-y",
        "40",
        "-e",
        &format!("GC_STATE_DIR={}", state.display()),
        "-e",
        &format!("GC_CONFIG={}", env.cfg.display()),
        "--",
        env!("CARGO_BIN_EXE_groundcontrol"),
    ]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut screen = String::new();
    while !(screen.contains("app") && screen.contains("config ok")) {
        assert!(Instant::now() < deadline, "TUI screen:\n{screen}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        screen = String::from_utf8_lossy(&tmux(&["capture-pane", "-p", "-t", "=tui:"]).stdout).into_owned();
    }
    assert!(screen.contains("dashboard"), "{screen}");
    assert!(!screen.to_lowercase().contains("daemon"), "{screen}");
    tmux(&["send-keys", "-t", "=tui:", "q"]);
}
