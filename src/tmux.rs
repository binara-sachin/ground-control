//! tmux control: every run and service lives in a window of a per-project
//! session, `gc-<project>` (docs/architecture.md, "tmux layout").
//!
//! Commands are passed to tmux as argv, after `--`, so tmux executes them
//! directly instead of through a shell. Targets always use exact matching
//! (`=session:=window`), so `api` never matches `api-local`.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Which tmux server to talk to.
#[derive(Debug, Clone, Default)]
pub struct Tmux {
    /// `-S <path>`; `None` is the user's default server.
    socket: Option<std::path::PathBuf>,
}

/// One window of a GroundControl session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// The window name as GroundControl gave it (unescaped).
    pub name: String,
    /// The window's command has exited (only seen with `remain-on-exit`).
    pub dead: bool,
    /// Exit status of a dead window's command; `None` when killed by a signal.
    pub dead_status: Option<i32>,
}

/// A window to create.
#[derive(Debug, Clone)]
pub struct NewWindow<'a> {
    pub project: &'a str,
    pub name: &'a str,
    pub cwd: &'a Path,
    /// Program and arguments, executed directly. Must not contain a lone `;`,
    /// which tmux would read as a command separator.
    pub argv: &'a [String],
    /// Extra environment for this window only (`-e`).
    pub env: &'a [(String, String)],
}

/// Session name for a project: `gc-<escaped id>`.
pub fn session_name(project: &str) -> String {
    format!("gc-{}", escape(project))
}

/// Exact-match target for a project's window.
pub fn window_target(project: &str, name: &str) -> String {
    format!("={}:={}", session_name(project), escape(name))
}

/// tmux rewrites `.` and `:` in session names, and `.` in a window name breaks
/// `session:window.pane` targets. Config ids allow letters, digits, `-`, `_`
/// and `.`, so `_` is doubled and `.` becomes `_d`, which keeps the mapping
/// one-to-one.
pub fn escape(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        match c {
            '_' => out.push_str("__"),
            '.' => out.push_str("_d"),
            c => out.push(c),
        }
    }
    out
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '_' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('_') => out.push('_'),
            Some('d') => out.push('.'),
            Some(other) => {
                out.push('_');
                out.push(other);
            }
            None => out.push('_'),
        }
    }
    out
}

/// `display-message` expands `#{…}` formats; `##` is a literal `#`.
pub fn escape_format(s: &str) -> String {
    s.replace('#', "##")
}

/// Parses `list-windows -F '#{window_name} #{pane_dead} #{pane_dead_status}'`.
/// tmux turns tabs into `_` in formats, so the fields are space-separated;
/// escaped names never contain spaces.
pub fn parse_windows(out: &str) -> Vec<WindowInfo> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split(' ');
            let name = it.next().filter(|n| !n.is_empty())?;
            let dead = it.next() == Some("1");
            let dead_status = if dead { it.next().and_then(|s| s.parse().ok()) } else { None };
            Some(WindowInfo { name: unescape(name), dead, dead_status })
        })
        .collect()
}

/// Errors tmux reports when the server, session or window simply isn't there.
fn is_missing(err: &str) -> bool {
    ["can't find session", "can't find window", "no server running", "error connecting to", "session not found"]
        .iter()
        .any(|m| err.contains(m))
}

impl Tmux {
    /// The user's default server, where the windows show up in `tmux ls`.
    pub fn default_server() -> Tmux {
        Tmux { socket: None }
    }

    /// A server on its own socket. The tests use this to stay isolated.
    pub fn with_socket(path: impl Into<std::path::PathBuf>) -> Tmux {
        Tmux { socket: Some(path.into()) }
    }

    async fn tmux(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new("tmux");
        if let Some(s) = &self.socket {
            cmd.arg("-S").arg(s);
        }
        cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(TIMEOUT, cmd.output())
            .await
            .map_err(|_| format!("tmux {} timed out", args.first().unwrap_or(&"")))?
            .map_err(|e| format!("cannot run tmux: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(err.lines().next().unwrap_or("tmux failed").trim().to_string())
        }
    }

    /// `tmux -V`, for the Health screen.
    pub async fn version(&self) -> Result<String, String> {
        self.tmux(&["-V"]).await.map(|s| s.trim().to_string())
    }

    /// The project's windows; empty when the session or the server is absent.
    pub async fn list_windows(&self, project: &str) -> Result<Vec<WindowInfo>, String> {
        let target = format!("={}", session_name(project));
        match self.tmux(&["list-windows", "-t", &target, "-F", "#{window_name} #{pane_dead} #{pane_dead_status}"]).await {
            Ok(out) => Ok(parse_windows(&out)),
            Err(e) if is_missing(&e) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub async fn window(&self, project: &str, name: &str) -> Result<Option<WindowInfo>, String> {
        Ok(self.list_windows(project).await?.into_iter().find(|w| w.name == name))
    }

    /// Opens a detached window in the project's session, creating the session
    /// if needed. The window keeps its pane after the command exits
    /// (`remain-on-exit`), so a failure's last output stays readable.
    ///
    /// tmux allows duplicate window names, so an existing window is refused
    /// here. The run and service locks are what guarantee single ownership;
    /// this only keeps the names unambiguous.
    pub async fn new_window(&self, w: &NewWindow<'_>) -> Result<(), String> {
        if self.window(w.project, w.name).await?.is_some() {
            return Err(format!("tmux window {} already exists", window_target(w.project, w.name)));
        }
        if w.argv.is_empty() || w.argv.iter().any(|a| a == ";") {
            return Err("window command must be non-empty and contain no `;` argument".into());
        }
        let session = session_name(w.project);
        let name = escape(w.name);
        let cwd = w.cwd.to_string_lossy();
        let target = window_target(w.project, w.name);
        let env: Vec<String> = w.env.iter().map(|(k, v)| format!("{k}={v}")).collect();

        let build = |create_session: bool| {
            let mut args: Vec<&str> =
                if create_session { vec!["new-session", "-d", "-s", &session] } else { vec!["new-window", "-d", "-t", &session] };
            args.extend(["-n", &name, "-c", &cwd]);
            for e in &env {
                args.extend(["-e", e]);
            }
            args.push("--");
            args.extend(w.argv.iter().map(String::as_str));
            // Chained in the same tmux invocation, so it applies before the
            // server can notice an instantly-exiting command.
            args.extend([";", "set-option", "-w", "-t", &target, "remain-on-exit", "on"]);
            args.into_iter().map(str::to_string).collect::<Vec<_>>()
        };

        let has_session = self.tmux(&["has-session", "-t", &format!("={session}")]).await.is_ok();
        let first = build(!has_session);
        match self.tmux(&first.iter().map(String::as_str).collect::<Vec<_>>()).await {
            Ok(_) => Ok(()),
            // Another TUI created the session between the check and now.
            Err(e) if !has_session && e.contains("duplicate session") => {
                let retry = build(false);
                self.tmux(&retry.iter().map(String::as_str).collect::<Vec<_>>()).await.map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    /// Closes one pane by its id (`$TMUX_PANE`, e.g. `%12`).
    pub async fn kill_pane(&self, pane_id: &str) -> Result<(), String> {
        self.tmux(&["kill-pane", "-t", pane_id]).await.map(|_| ())
    }

    /// Shows `msg` in the status line of every attached client, whatever
    /// session it is looking at. A bell in a detached `gc-*` session is only
    /// flagged there, so this is how a worker gets noticed. No clients: no-op.
    pub async fn tell_clients(&self, msg: &str) -> Result<(), String> {
        let clients = match self.tmux(&["list-clients", "-F", "#{client_name}"]).await {
            Ok(out) => out,
            Err(e) if is_missing(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let text = format!("GroundControl: {}", escape_format(msg));
        for c in clients.lines().filter(|c| !c.is_empty()) {
            let _ = self.tmux(&["display-message", "-c", c, "-d", "8000", &text]).await;
        }
        Ok(())
    }

    /// Sends keys such as `C-c` to a window. A missing window is an error.
    pub async fn send_keys(&self, project: &str, name: &str, keys: &[&str]) -> Result<(), String> {
        let target = window_target(project, name);
        let mut args = vec!["send-keys", "-t", &target];
        args.extend(keys);
        self.tmux(&args).await.map(|_| ())
    }

    /// Closes a window. Closing one that is already gone is not an error.
    pub async fn kill_window(&self, project: &str, name: &str) -> Result<(), String> {
        match self.tmux(&["kill-window", "-t", &window_target(project, name)]).await {
            Ok(_) => Ok(()),
            Err(e) if is_missing(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The text of a window's pane, scrollback included (a dead pane's
    /// "Pane is dead" line pushes the top of the screen into history). Used by
    /// the tests and for diagnostics; the TUI reads log files instead.
    pub async fn capture(&self, project: &str, name: &str) -> Result<String, String> {
        self.tmux(&["capture-pane", "-p", "-J", "-S", "-", "-t", &window_target(project, name)]).await
    }

    /// Stops this server. Only for servers made with `with_socket`.
    pub async fn kill_server(&self) {
        if self.socket.is_some() {
            let _ = self.tmux(&["kill-server"]).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_is_one_to_one() {
        for id in ["api", "api-local", "a.b", "a_b", "a_d", "a__b", "x._.y", "_", "."] {
            assert_eq!(unescape(&escape(id)), id, "{id}");
            assert!(!escape(id).contains('.'), "{id}");
        }
        assert_ne!(escape("a.b"), escape("a_b"));
        assert_ne!(escape("a.b"), escape("a_d"));
        assert_eq!(session_name("git.internals"), "gc-git_dinternals");
        assert_eq!(window_target("p", "run-412"), "=gc-p:=run-412");
    }

    #[test]
    fn parses_window_list() {
        let out = "api-local 0 \nrun-412 1 3\nweb 1 \n\n";
        assert_eq!(
            parse_windows(out),
            vec![
                WindowInfo { name: "api-local".into(), dead: false, dead_status: None },
                WindowInfo { name: "run-412".into(), dead: true, dead_status: Some(3) },
                WindowInfo { name: "web".into(), dead: true, dead_status: None },
            ]
        );
        assert_eq!(parse_windows("a__b_dc 0 ")[0].name, "a_b.c");
    }

    #[test]
    fn format_escaping() {
        assert_eq!(escape_format("run #412 needs #{approval}"), "run ##412 needs ##{approval}");
    }

    #[test]
    fn missing_errors() {
        assert!(is_missing("can't find session: gc-x"));
        assert!(is_missing("no server running on /tmp/tmux-501/default"));
        assert!(!is_missing("unknown option -- z"));
    }
}
