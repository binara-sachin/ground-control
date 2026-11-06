//! Drives a real tmux server on a private socket. Skipped when tmux is not
//! installed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use groundcontrol::tmux::{NewWindow, Tmux, WindowInfo};

struct Server {
    tmux: Tmux,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Server {
    fn start() -> Option<Server> {
        if std::process::Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("tmux not installed; skipping");
            return None;
        }
        // Unix socket paths are limited to ~104 bytes, so keep this short.
        let dir = tempfile::Builder::new().prefix("gct").tempdir_in(std::env::temp_dir()).unwrap();
        let socket = dir.path().join("s");
        Some(Server { tmux: Tmux::with_socket(&socket), socket, _dir: dir })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux").arg("-S").arg(&self.socket).arg("kill-server").output();
    }
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

async fn open(t: &Tmux, project: &str, name: &str, cmd: &[&str], env: &[(String, String)]) -> Result<(), String> {
    t.new_window(&NewWindow { project, name, cwd: Path::new("/"), argv: &argv(cmd), env }).await
}

async fn wait_dead(t: &Tmux, project: &str, name: &str) -> WindowInfo {
    for _ in 0..100 {
        if let Some(w) = t.window(project, name).await.unwrap()
            && w.dead
        {
            return w;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("window {name} did not exit");
}

#[tokio::test]
async fn windows_lifecycle() {
    let Some(s) = Server::start() else { return };
    let t = &s.tmux;

    // No server yet: nothing listed, closing is a no-op.
    assert_eq!(t.list_windows("p").await.unwrap(), vec![]);
    t.kill_window("p", "nope").await.unwrap();

    // The first window creates the session; env reaches it; the pane survives exit.
    let env = vec![("GC_X".to_string(), "hello".to_string())];
    open(t, "p", "api", &["sh", "-c", "echo \"x=$GC_X\"; exit 3"], &env).await.unwrap();
    let w = wait_dead(t, "p", "api").await;
    assert_eq!(w.dead_status, Some(3));
    assert!(t.capture("p", "api").await.unwrap().contains("x=hello"));

    // Arguments are passed as argv, never through a shell.
    open(t, "p", "argv", &["printf", "%s\\n", "a b;c $HOME `id`"], &[]).await.unwrap();
    wait_dead(t, "p", "argv").await;
    assert!(t.capture("p", "argv").await.unwrap().contains("a b;c $HOME `id`"));

    // A second window joins the session; a duplicate name is refused.
    open(t, "p", "web", &["sleep", "30"], &[]).await.unwrap();
    let err = open(t, "p", "web", &["sleep", "30"], &[]).await.unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    let names: Vec<String> = t.list_windows("p").await.unwrap().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["api", "argv", "web"]);
    assert!(!t.window("p", "web").await.unwrap().unwrap().dead);

    // Exact targets: `web` is not matched by a prefix.
    assert!(t.send_keys("p", "we", &["C-c"]).await.is_err());

    // C-c stops the command; the dead pane stays until it is closed.
    t.send_keys("p", "web", &["C-c"]).await.unwrap();
    let w = wait_dead(t, "p", "web").await;
    assert_eq!(w.dead_status, None, "killed by a signal");
    t.kill_window("p", "web").await.unwrap();
    assert!(t.window("p", "web").await.unwrap().is_none());

    // A lone `;` would split the tmux command, so it is refused.
    assert!(open(t, "p", "semi", &["echo", ";"], &[]).await.is_err());

    assert!(t.version().await.unwrap().starts_with("tmux "));
}

#[tokio::test]
async fn ids_with_dots_and_underscores() {
    let Some(s) = Server::start() else { return };
    let t = &s.tmux;

    open(t, "git.internals", "api.v2", &["sleep", "30"], &[]).await.unwrap();
    open(t, "git_internals", "api_v2", &["sleep", "30"], &[]).await.unwrap();

    let a = t.list_windows("git.internals").await.unwrap();
    let b = t.list_windows("git_internals").await.unwrap();
    assert_eq!(a.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), vec!["api.v2"]);
    assert_eq!(b.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), vec!["api_v2"]);

    t.kill_window("git.internals", "api.v2").await.unwrap();
    assert!(t.window("git.internals", "api.v2").await.unwrap().is_none());
    assert!(t.window("git_internals", "api_v2").await.unwrap().is_some());
    t.kill_server().await;
}
