//! Shared setup for the end-to-end tests: a private tmux server, a git repo
//! and a config, all in a temporary folder.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use groundcontrol::config::Config;
use groundcontrol::ctx::Ctx;
use groundcontrol::tmux::{Tmux, Window};
use groundcontrol::window::{self, RunWindow, Windows};

pub struct Env {
    pub ctx: Ctx,
    pub config: Config,
    pub socket: PathBuf,
    pub base: PathBuf,
    pub cfg_path: PathBuf,
    pub port: u16,
    _dir: tempfile::TempDir,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new("tmux").arg("-S").arg(&self.socket).arg("kill-server").output();
    }
}

pub fn sh(dir: &Path, cmd: &str) {
    let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status().unwrap().success();
    assert!(ok, "{cmd}");
}

pub fn git_repo(dir: &Path, remote: &str) {
    std::fs::create_dir_all(dir).unwrap();
    sh(
        dir,
        &format!(
            "git init -q -b main && git config user.email t@t && git config user.name t && \
             git remote add origin {remote} && printf 'secrets.env\\n' > .gitignore && \
             echo hi > README && git add -A && git commit -qm init"
        ),
    );
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub fn tmux_installed() -> bool {
    let ok = Command::new("tmux").arg("-V").output().is_ok();
    if !ok {
        eprintln!("tmux not installed; skipping");
    }
    ok
}

/// A temporary folder with `app/` (a git repo whose origin is
/// `github.com/me/app` and which has `secrets.env`) and the config that
/// `config(repo, port)` returns.
pub fn setup(config: impl Fn(&Path, u16) -> String) -> Option<Env> {
    if !tmux_installed() {
        return None;
    }
    // Short: the tmux socket path must stay under ~104 bytes.
    let dir = tempfile::Builder::new().prefix("gc").tempdir_in(std::env::temp_dir()).unwrap();
    let base = dir.path().to_path_buf();
    let repo = base.join("app");
    git_repo(&repo, "git@github.com:me/app.git");
    std::fs::write(repo.join("secrets.env"), "API_TOKEN=sup3r-s3cret-value\n").unwrap();
    let port = free_port();
    let cfg = config(&repo, port);
    let cfg_path = base.join("groundcontrol.yaml");
    std::fs::write(&cfg_path, &cfg).unwrap();
    let config = groundcontrol::config::load_str(&cfg).config.expect("valid config");
    let socket = base.join("s");
    let ctx = Ctx::new(Tmux::with_socket(&socket), env!("CARGO_BIN_EXE_groundcontrol").into(), cfg_path.clone()).unwrap();
    Some(Env { ctx, config, socket, base, cfg_path, port, _dir: dir })
}

impl Env {
    pub fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux").arg("-S").arg(&self.socket).args(args).output().unwrap()
    }

    /// The GroundControl windows of the session.
    pub async fn windows(&self) -> Windows {
        window::list(&self.ctx.tmux).await.unwrap()
    }

    pub async fn run_window(&self, id: i64) -> Option<RunWindow> {
        window::run(&self.ctx.tmux, id).await.unwrap()
    }

    /// Waits up to `secs` for `want`, which gets every window of the session
    /// (without options).
    pub async fn wait_for(&self, secs: u64, what: &str, want: impl Fn(&[Window]) -> bool) -> Vec<Window> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let w = self.ctx.tmux.windows(&[]).await.unwrap();
            if want(&w) {
                return w;
            }
            assert!(Instant::now() < deadline, "{what}: {w:#?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
