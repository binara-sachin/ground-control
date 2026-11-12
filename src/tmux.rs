//! tmux control. Every run and service is a window of the `ground-control`
//! session, created on first use (docs/architecture.md, "tmux").
//!
//! tmux is also where all state lives: each window carries its kind, its
//! project and its live status as `@gc_*` window options, in the tmux
//! server's memory only. Nothing is written to disk. Which options exist is
//! `crate::window`'s business; this module only moves them.
//!
//! Commands are passed to tmux as argv, after `--`, so tmux executes them
//! directly instead of through a shell. Windows are addressed by their id
//! (`@12`), which tmux never reuses while the server runs.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

pub const SESSION: &str = "ground-control";
const TIMEOUT: Duration = Duration::from_secs(5);
/// Scrollback of the session's windows: it is the only copy of their output.
const HISTORY_LIMIT: &str = "50000";

/// Which tmux server to talk to.
#[derive(Debug, Clone, Default)]
pub struct Tmux {
    /// `-S <path>`; `None` is the user's default server (inside a pane: the
    /// pane's own server, through `$TMUX`).
    socket: Option<std::path::PathBuf>,
}

/// One window of the `ground-control` session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The number of the window id (`@12` → 12).
    pub id: i64,
    /// The window's command has exited (seen thanks to `remain-on-exit`).
    pub dead: bool,
    /// Exit status of a dead window's command; `None` when killed by a signal.
    pub dead_status: Option<i32>,
    /// The decoded `@gc_<key>` options that were asked for; unset ones are
    /// empty. What they mean is `crate::window`'s business.
    pub opts: HashMap<String, String>,
}

/// A window to create.
#[derive(Debug, Clone)]
pub struct NewWindow<'a> {
    /// Display name, for example `app/web`. Need not be unique.
    pub name: &'a str,
    pub cwd: &'a Path,
    /// Program and arguments, executed directly. Must not contain a lone `;`,
    /// which tmux would read as a command separator.
    pub argv: &'a [String],
    /// Extra environment for this window only (`-e`).
    pub env: &'a [(String, String)],
    /// `@gc_<key>` options, set before the command can read them.
    pub options: &'a [(&'a str, String)],
}

/// The `wait-for` channel a window's worker blocks on between requests.
pub fn wake_channel(id: i64) -> String {
    format!("gc-wake-{id}")
}

/// `@12` for window 12.
pub fn target(id: i64) -> String {
    format!("@{id}")
}

/// Option values are percent-encoded where tmux or GroundControl would
/// misread them: tmux turns control characters (newlines included), and any
/// non-ASCII character when its locale isn't UTF-8, into `_`; `|` and `,`
/// separate fields in what GroundControl reads back.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '|' | ',' | ';' => out.push_str(&format!("%{:02X}", c as u32)),
            c if c.is_control() || !c.is_ascii() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `display-message` expands `#{…}` formats; `##` is a literal `#`.
fn escape_format(s: &str) -> String {
    s.replace('#', "##")
}

fn list_format(keys: &[&str]) -> String {
    let mut f = String::from("#{window_id}|#{pane_dead}|#{pane_dead_status}");
    for k in keys {
        f.push_str(&format!("|#{{@gc_{k}}}"));
    }
    f
}

/// Parses `list-windows -F <list_format(keys)>`.
fn parse_windows(out: &str, keys: &[&str]) -> Vec<Window> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split('|');
            let id = it.next()?.strip_prefix('@')?.parse().ok()?;
            let dead = it.next() == Some("1");
            let status = it.next().and_then(|s| s.parse().ok());
            let opts = keys.iter().zip(it).map(|(k, v)| (k.to_string(), decode(v))).collect();
            Some(Window { id, dead, dead_status: if dead { status } else { None }, opts })
        })
        .collect()
}

/// Errors tmux reports when the server, session or window simply isn't there.
fn is_missing(err: &str) -> bool {
    [
        "can't find session",
        "can't find window",
        "can't find pane",
        "no server running",
        "error connecting to",
        "session not found",
        // The server is still up but its last session just closed.
        "no current target",
    ]
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
        self.run(args, Some(TIMEOUT)).await
    }

    async fn run(&self, args: &[&str], timeout: Option<Duration>) -> Result<String, String> {
        let mut cmd = Command::new("tmux");
        if let Some(s) = &self.socket {
            cmd.arg("-S").arg(s);
        }
        cmd.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
        let output = cmd.output();
        let out = match timeout {
            Some(t) => tokio::time::timeout(t, output).await.map_err(|_| format!("tmux {} timed out", args.first().unwrap_or(&"")))?,
            None => output.await,
        }
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

    /// Every window of the `ground-control` session with the `@gc_<key>`
    /// options asked for; empty when the session or the server is absent.
    pub async fn windows(&self, keys: &[&str]) -> Result<Vec<Window>, String> {
        match self.tmux(&["list-windows", "-t", &format!("={SESSION}:"), "-F", &list_format(keys)]).await {
            Ok(out) => Ok(parse_windows(&out, keys)),
            Err(e) if is_missing(&e) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub async fn window(&self, id: i64, keys: &[&str]) -> Result<Option<Window>, String> {
        Ok(self.windows(keys).await?.into_iter().find(|w| w.id == id))
    }

    /// Opens a detached window in the `ground-control` session, creating the
    /// session if it doesn't exist. Returns the window's id.
    ///
    /// The window is created under a unique temporary name so that, in the
    /// same tmux command, its options can be set by exact name before its
    /// command runs; then it is renamed. `remain-on-exit` keeps the pane after
    /// the command exits, so a failure's last output stays readable.
    pub async fn new_window(&self, w: &NewWindow<'_>) -> Result<i64, String> {
        if w.argv.is_empty() || w.argv.iter().any(|a| a == ";") {
            return Err("window command must be non-empty and contain no `;` argument".into());
        }
        let temp = format!("gc-new-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
        let temp_target = format!("={SESSION}:={temp}");
        let session_target = format!("={SESSION}:");
        let cwd = w.cwd.to_string_lossy();
        let env: Vec<String> = w.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let opts: Vec<(String, String)> = w.options.iter().map(|(k, v)| (format!("@gc_{k}"), encode(v))).collect();

        let build = |create_session: bool| {
            let mut args: Vec<&str> =
                if create_session { vec!["new-session", "-d", "-s", SESSION] } else { vec!["new-window", "-d", "-t", &session_target] };
            args.extend(["-n", &temp, "-P", "-F", "#{window_id}", "-c", &cwd]);
            for e in &env {
                args.extend(["-e", e]);
            }
            args.push("--");
            args.extend(w.argv.iter().map(String::as_str));
            args.extend([";", "set-option", "-w", "-t", &temp_target, "remain-on-exit", "on"]);
            for (k, v) in &opts {
                args.extend([";", "set-option", "-w", "-t", &temp_target, k, v]);
            }
            // Later windows keep more output; the session's first pane already exists.
            args.extend([";", "set-option", "-t", &session_target, "history-limit", HISTORY_LIMIT]);
            args.extend([";", "rename-window", "-t", &temp_target, w.name]);
            args.into_iter().map(str::to_string).collect::<Vec<_>>()
        };

        let has_session = self.tmux(&["has-session", "-t", &format!("={SESSION}")]).await.is_ok();
        let run = |args: Vec<String>| async move { self.tmux(&args.iter().map(String::as_str).collect::<Vec<_>>()).await };
        let out = match run(build(!has_session)).await {
            Ok(out) => out,
            // Another TUI created the session between the check and now.
            Err(e) if !has_session && e.contains("duplicate session") => run(build(false)).await?,
            Err(e) => return Err(e),
        };
        out.lines()
            .find_map(|l| l.trim().strip_prefix('@').and_then(|n| n.parse().ok()))
            .ok_or_else(|| format!("tmux did not report the new window's id ({})", out.trim()))
    }

    /// The id of the window a pane (`%5`) is in.
    pub async fn pane_window(&self, pane: &str) -> Result<i64, String> {
        let out = self.tmux(&["display-message", "-p", "-t", pane, "#{window_id}"]).await?;
        out.trim().strip_prefix('@').and_then(|n| n.parse().ok()).ok_or_else(|| format!("unexpected window id `{}`", out.trim()))
    }

    /// Sets `@gc_<key>` options on a window (`@12`) or on the window of a pane (`%5`).
    pub async fn set_options(&self, target: &str, options: &[(&str, String)]) -> Result<(), String> {
        let pairs: Vec<(String, String)> = options.iter().map(|(k, v)| (format!("@gc_{k}"), encode(v))).collect();
        let mut args: Vec<&str> = Vec::new();
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                args.push(";");
            }
            args.extend(["set-option", "-w", "-t", target, k, v]);
        }
        if args.is_empty() {
            return Ok(());
        }
        self.tmux(&args).await.map(|_| ())
    }

    /// Sets `@gc_<key>` options on window `id` and wakes its worker, in one
    /// tmux command: how the TUI sends a request.
    pub async fn request(&self, id: i64, options: &[(&str, String)]) -> Result<(), String> {
        let t = target(id);
        let channel = wake_channel(id);
        let pairs: Vec<(String, String)> = options.iter().map(|(k, v)| (format!("@gc_{k}"), encode(v))).collect();
        let mut args: Vec<&str> = Vec::new();
        for (k, v) in &pairs {
            args.extend(["set-option", "-w", "-t", &t, k, v, ";"]);
        }
        args.extend(["wait-for", "-S", &channel]);
        self.tmux(&args).await.map(|_| ())
    }

    /// Blocks until `channel` is signalled (`wait-for -S`), with no timeout. A
    /// signal sent while nobody waits is remembered, so none is lost between
    /// two waits. Returns an error when the server goes away.
    pub async fn wait_for(&self, channel: &str) -> Result<(), String> {
        self.run(&["wait-for", channel], None).await.map(|_| ())
    }

    /// Reads `@gc_<key>` options of a window or of a pane's window, decoded,
    /// in the order asked. Missing options are empty.
    pub async fn options(&self, target: &str, keys: &[&str]) -> Result<Vec<String>, String> {
        let format: Vec<String> = keys.iter().map(|k| format!("#{{@gc_{k}}}")).collect();
        let out = self.tmux(&["display-message", "-p", "-t", target, &format.join("|")]).await?;
        let line = out.lines().next().unwrap_or("");
        let mut values: Vec<String> = line.split('|').map(decode).collect();
        values.resize(keys.len(), String::new());
        Ok(values)
    }

    /// Sends keys such as `C-c` to a window. A missing window is an error.
    pub async fn send_keys(&self, id: i64, keys: &[&str]) -> Result<(), String> {
        let t = target(id);
        let mut args = vec!["send-keys", "-t", &t];
        args.extend(keys);
        self.tmux(&args).await.map(|_| ())
    }

    /// Closes a window. Closing one that is already gone is not an error.
    pub async fn kill_window(&self, id: i64) -> Result<(), String> {
        match self.tmux(&["kill-window", "-t", &target(id)]).await {
            Ok(_) => Ok(()),
            Err(e) if is_missing(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Closes one pane by its id (`$TMUX_PANE`, e.g. `%12`).
    pub async fn kill_pane(&self, pane_id: &str) -> Result<(), String> {
        self.tmux(&["kill-pane", "-t", pane_id]).await.map(|_| ())
    }

    /// The text of a window's pane, scrollback included, with colour escapes
    /// and wrapped lines joined. This is the only copy of a window's output.
    pub async fn capture(&self, id: i64) -> Result<String, String> {
        self.tmux(&["capture-pane", "-p", "-e", "-J", "-S", "-", "-t", &target(id)]).await
    }

    /// Shows `msg` in the status line of every attached client, whatever
    /// session it is looking at. A bell in the detached `ground-control`
    /// session is only flagged there, so this is how a worker gets noticed.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips() {
        for s in ["plain", "a|b,c;d", "100% done", "line1\nline2\r\n", "tab\there", "é ✓", ""] {
            let e = encode(s);
            assert!(!e.contains('|') && !e.contains(',') && e.chars().all(|c| c.is_ascii() && !c.is_control()), "{e:?}");
            assert_eq!(decode(&e), s);
        }
        assert_eq!(decode("50%"), "50%", "a stray % is kept");
    }

    #[test]
    fn parses_window_list() {
        let keys = ["kind", "msg", "project"];
        let w = &parse_windows("@12|1|3|svc|port%7C8080|\n@13|0|\n", &keys)[..];
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].id, w[0].dead, w[0].dead_status), (12, true, Some(3)));
        assert_eq!(w[0].opts["kind"], "svc");
        assert_eq!(w[0].opts["msg"], "port|8080");
        assert_eq!(w[0].opts["project"], "");
        assert_eq!((w[1].id, w[1].dead, w[1].opts.get("kind")), (13, false, None));
    }

    #[test]
    fn format_escaping() {
        assert_eq!(escape_format("run #412 needs #{approval}"), "run ##412 needs ##{approval}");
    }

    #[test]
    fn missing_errors() {
        assert!(is_missing("can't find session: ground-control"));
        assert!(is_missing("no server running on /tmp/tmux-501/default"));
        assert!(!is_missing("unknown option -- z"));
    }
}
