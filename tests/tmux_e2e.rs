//! Drives a real tmux server on a private socket: the `ground-control`
//! session, windows and their `@gc_*` options. Skipped when tmux is not
//! installed.

mod common;

use std::path::Path;

use groundcontrol::tmux::{NewWindow, SESSION, target};

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn session_windows_and_options() {
    let Some(env) = common::setup(|_, _| "projects: {}\n".into()) else { return };
    let t = &env.ctx.tmux;

    // No server yet: nothing listed, closing is a no-op.
    assert!(t.windows().await.unwrap().is_empty());
    t.kill_window(999).await.unwrap();

    // The first window creates the `ground-control` session. Its options are
    // set before its command runs, the env reaches it and the pane survives exit.
    let tricky = "a|b,c;d 100% \u{e9}\nnext line".to_string();
    let env_vars = vec![("GC_X".to_string(), "hello".to_string())];
    let options = [("kind", "test".to_string()), ("msg", tricky.clone())];
    let cmd = argv(&["sh", "-c", "tmux display -p -t \"$TMUX_PANE\" 'kind=#{@gc_kind}'; echo \"x=$GC_X\"; exit 3"]);
    let first =
        t.new_window(&NewWindow { name: "app/web", cwd: Path::new("/"), argv: &cmd, env: &env_vars, options: &options }).await.unwrap();
    let w = env.wait_for(5, "first window exits", |w| w.iter().any(|w| w.id == first && w.dead)).await;
    let w = w.iter().find(|w| w.id == first).unwrap();
    assert_eq!(w.dead_status, Some(3));
    assert_eq!(w.opt("kind"), Some("test"));
    assert_eq!(w.opt("msg"), Some(tricky.as_str()), "values survive tmux untouched");
    let pane = t.capture(first).await.unwrap();
    assert!(pane.contains("kind=test"), "options were set before the command ran: {pane}");
    assert!(pane.contains("x=hello"), "{pane}");
    let sessions = String::from_utf8(env.tmux(&["list-sessions", "-F", "#{session_name}"]).stdout).unwrap();
    assert_eq!(sessions.trim(), SESSION);
    let name = String::from_utf8(env.tmux(&["display", "-p", "-t", &target(first), "#{window_name}"]).stdout).unwrap();
    assert_eq!(name.trim(), "app/web");

    // Arguments are passed as argv, never through a shell.
    let cmd = argv(&["printf", "%s\\n", "a b;c $HOME `id`"]);
    let id = t.new_window(&NewWindow { name: "argv", cwd: Path::new("/"), argv: &cmd, env: &[], options: &[] }).await.unwrap();
    env.wait_for(5, "argv window exits", |w| w.iter().any(|w| w.id == id && w.dead)).await;
    assert!(t.capture(id).await.unwrap().contains("a b;c $HOME `id`"));

    // A second window joins the existing session; options can be read and written by id.
    let cmd = argv(&["sleep", "30"]);
    let live = t.new_window(&NewWindow { name: "app/web", cwd: Path::new("/"), argv: &cmd, env: &[], options: &[] }).await.unwrap();
    assert!(live > id);
    let sessions = String::from_utf8(env.tmux(&["list-sessions", "-F", "#{session_name}"]).stdout).unwrap();
    assert_eq!(sessions.lines().count(), 1, "windows share the one session");
    t.set_options(&target(live), &[("state", "running".into()), ("msg", "x|y".into())]).await.unwrap();
    assert_eq!(t.options(&target(live), &["state", "msg", "missing"]).await.unwrap(), vec!["running", "x|y", ""]);
    let pane_id = String::from_utf8(env.tmux(&["display", "-p", "-t", &target(live), "#{pane_id}"]).stdout).unwrap();
    assert_eq!(t.pane_window(pane_id.trim()).await.unwrap(), live);

    // C-c stops the command; the dead pane stays until it is closed.
    t.send_keys(live, &["C-c"]).await.unwrap();
    let w = env.wait_for(5, "C-c", |w| w.iter().any(|w| w.id == live && w.dead)).await;
    assert_eq!(w.iter().find(|w| w.id == live).unwrap().dead_status, None, "killed by a signal");
    t.kill_window(live).await.unwrap();
    assert!(t.window(live).await.unwrap().is_none());

    // A lone `;` would split the tmux command, so it is refused.
    let cmd = argv(&["echo", ";"]);
    assert!(t.new_window(&NewWindow { name: "semi", cwd: Path::new("/"), argv: &cmd, env: &[], options: &[] }).await.is_err());

    assert!(t.version().await.unwrap().starts_with("tmux "));
}
