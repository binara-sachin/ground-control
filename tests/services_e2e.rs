//! End-to-end tests for daemonless services: real `groundcontrol svc-exec`
//! workers in windows of a private tmux server, driven through
//! `services::control`. Skipped when tmux is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use groundcontrol::config::{Config, ServiceAction};
use groundcontrol::protocol::{ServiceState, ServiceStatus};
use groundcontrol::services::{self, Ctx};
use groundcontrol::tmux::Tmux;

struct Env {
    ctx: Ctx,
    config: Config,
    socket: PathBuf,
    state: PathBuf,
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn setup() -> Option<Env> {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return None;
    }
    // Short: the tmux socket path must stay under ~104 bytes.
    let dir = tempfile::Builder::new().prefix("gcs").tempdir_in(std::env::temp_dir()).unwrap();
    let repo = dir.path().join("app");
    std::fs::create_dir_all(&repo).unwrap();
    sh(
        &repo,
        "git init -q -b main && git config user.email t@t && git config user.name t && \
         git remote add origin git@github.com:me/app.git && printf 'secrets.env\\n' > .gitignore && \
         echo hi > README && git add -A && git commit -qm init",
    );
    std::fs::write(repo.join("secrets.env"), "API_TOKEN=sup3r-s3cret-value\n").unwrap();
    let port = free_port();
    let cfg = format!(
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
    );
    let cfg_path = dir.path().join("groundcontrol.yaml");
    std::fs::write(&cfg_path, &cfg).unwrap();
    let config = groundcontrol::config::load_str(&cfg).config.expect("valid config");
    let socket = dir.path().join("s");
    let state = dir.path().join("state");
    let ctx = Ctx::new(Tmux::with_socket(&socket), env!("CARGO_BIN_EXE_groundcontrol").into(), state.clone(), cfg_path).unwrap();
    Some(Env { ctx, config, socket, state, port, _dir: dir })
}

impl Env {
    async fn control(&self, target: &str, action: ServiceAction) -> Result<String, String> {
        services::control(&self.ctx, &self.config, "app", target, action).await
    }

    fn status(&self, id: &str) -> ServiceStatus {
        let p = self.config.project("app").unwrap();
        services::status(&self.ctx, p, p.service(id).unwrap())
    }

    async fn wait_status(&self, id: &str, want: impl Fn(&ServiceStatus) -> bool) -> ServiceStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = self.status(id);
            if want(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "{id} did not reach the expected state: {s:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn windows(&self) -> Vec<String> {
        let out = Command::new("tmux").arg("-S").arg(&self.socket).args(["list-windows", "-t", "=gc-app", "-F", "#{window_name}"]).output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect()).unwrap_or_default()
    }

    fn tmux(&self, args: &[&str]) {
        let ok = Command::new("tmux").arg("-S").arg(&self.socket).args(args).status().unwrap().success();
        assert!(ok, "tmux {args:?}");
    }

    fn log(&self, service: &str) -> String {
        std::fs::read_to_string(self.state.join("services/app").join(format!("{service}.log"))).unwrap_or_default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn services_end_to_end() {
    let Some(env) = setup() else { return };
    let port = env.port;

    // Port in use by something GroundControl did not start.
    let blocker = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let e = env.control("web", ServiceAction::Start).await.unwrap_err();
    assert!(e.contains(&format!("port {port} is in use")), "{e}");
    assert!(e.contains("not started by GroundControl"), "{e}");
    assert_eq!(env.status("web").state, ServiceState::Stopped);
    drop(blocker);

    // Start with an HTTP health check. The refused launch's dead window is replaced.
    env.control("web", ServiceAction::Start).await.unwrap();
    let web = env.status("web");
    assert_eq!(web.state, ServiceState::Running);
    assert!(web.healthy);
    assert!(web.pid.is_some());
    assert_eq!(env.windows(), ["svc-web"]);

    // Starting it again is a no-op.
    env.control("web", ServiceAction::Start).await.unwrap();
    assert_eq!(env.windows(), ["svc-web"]);

    // restart: never with a non-zero exit is reported as a failed start.
    let e = env.control("oneshot", ServiceAction::Start).await.unwrap_err();
    assert!(e.contains("exit code 4"), "{e}");
    assert_eq!(env.status("oneshot").state, ServiceState::Crashed);

    // on-failure restarts with back-off.
    env.control("worker", ServiceAction::Start).await.unwrap();
    let w = env.wait_status("worker", |s| s.restart_count >= 1).await;
    assert!(matches!(w.state, ServiceState::Restarting | ServiceState::Starting | ServiceState::Running), "{w:?}");

    // Group stop: everything stopped, windows closed, port free.
    env.control("stack", ServiceAction::Stop).await.unwrap();
    assert_eq!(env.status("web").state, ServiceState::Stopped);
    assert_eq!(env.status("worker").state, ServiceState::Stopped);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "web must be stopped");
    assert!(!env.windows().iter().any(|w| w == "svc-web" || w == "svc-worker"), "{:?}", env.windows());

    // Service log captured.
    assert!(env.log("web").contains("starting: python3"), "{}", env.log("web"));
}

#[tokio::test(flavor = "multi_thread")]
async fn masking_and_stop_paths() {
    let Some(env) = setup() else { return };

    // env_file values are masked in the pane and in the log.
    env.control("secret", ServiceAction::Start).await.unwrap();
    let mut pane = String::new();
    for _ in 0..50 {
        pane = env.ctx.tmux.capture("app", "svc-secret").await.unwrap();
        if pane.contains("token=") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(pane.contains("token="), "{pane}");
    assert!(!pane.contains("sup3r-s3cret-value"), "{pane}");
    assert!(env.log("secret").contains("token="));
    assert!(!env.log("secret").contains("sup3r-s3cret-value"));

    // A stop works even while the window is scrolled back in copy mode,
    // where C-c would go to tmux instead of the worker.
    env.tmux(&["copy-mode", "-t", "=gc-app:=svc-secret"]);
    let t = Instant::now();
    env.control("secret", ServiceAction::Stop).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(10), "stop took {:?}", t.elapsed());
    assert_eq!(env.status("secret").state, ServiceState::Stopped);

    // C-c in the window stops it by hand; the dead window stays readable.
    env.control("sleeper", ServiceAction::Start).await.unwrap();
    env.ctx.tmux.send_keys("app", "svc-sleeper", &["C-c"]).await.unwrap();
    let s = env.wait_status("sleeper", |s| s.state == ServiceState::Stopped).await;
    assert_eq!(s.message, None);
    assert!(env.ctx.tmux.window("app", "svc-sleeper").await.unwrap().unwrap().dead);

    // A second worker for a running service gives up and closes its own window.
    env.control("sleeper", ServiceAction::Start).await.unwrap();
    let pid = env.status("sleeper").pid.unwrap();
    let exe = env!("CARGO_BIN_EXE_groundcontrol");
    env.tmux(&[
        "new-window",
        "-d",
        "-t",
        "=gc-app",
        "-n",
        "svc-dup",
        "-e",
        &format!("GC_STATE_DIR={}", env.state.display()),
        "--",
        exe,
        "svc-exec",
        "app",
        "sleeper",
        "dup",
    ]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.windows().iter().any(|w| w == "svc-dup") {
        assert!(Instant::now() < deadline, "duplicate worker window should close itself");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(env.status("sleeper").state, ServiceState::Running);

    // Closing the window (SIGHUP) still ends the service's process group.
    env.ctx.tmux.kill_window("app", "svc-sleeper").await.unwrap();
    let s = env.wait_status("sleeper", |s| s.state == ServiceState::Stopped).await;
    assert_eq!(s.pid, None);
    let alive = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), None).is_ok();
    assert!(!alive, "the service's process group must be gone");
}
