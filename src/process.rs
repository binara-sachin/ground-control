//! Spawning shell commands in their own session and process group with stdout and stderr
//! merged into one ordered stream (§6.1, §5.1 Log writer).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

pub const SHELL: &str = "/bin/zsh";
pub const KILL_GRACE: Duration = Duration::from_secs(10);

pub struct Spawned {
    pub child: Child,
    pub pid: i32,
    /// Raw output chunks in the order the process wrote them. Closed at EOF.
    pub output: mpsc::Receiver<Vec<u8>>,
}

/// Runs `/bin/zsh -c <command>` in `dir`, stdin from `/dev/null`, in a new
/// session and process group whose id equals the child's pid. Not a login
/// shell: tmux-hosted workers already inherit the user's interactive
/// environment.
pub fn spawn_shell(command: &str, dir: &Path, env: &BTreeMap<String, String>) -> std::io::Result<Spawned> {
    spawn(&["-c", command], dir, env)
}

fn spawn(args: &[&str], dir: &Path, env: &BTreeMap<String, String>) -> std::io::Result<Spawned> {
    let (reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new(SHELL);
    cmd.args(args)
        .current_dir(dir)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .stderr(Stdio::from(writer))
        .kill_on_drop(false);
    // A new session: the child leads its own process group (pgid == pid) and
    // has no controlling terminal. Inside a tmux pane this keeps steps from
    // reading the pane's size or prompting on `/dev/tty`, as a detached process would be.
    // SAFETY: `setsid` is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::from));
    }
    let child = cmd.spawn()?;
    // Dropping `cmd` closes our copies of the write end, so EOF arrives once
    // the child (and anything it left running) has closed its copies.
    drop(cmd);
    let pid = child.id().map(|p| p as i32).unwrap_or(-1);
    let (tx, rx) = mpsc::channel(256);
    std::thread::Builder::new().name(format!("gc-output-{pid}")).spawn(move || pump(reader, tx))?;
    Ok(Spawned { child, pid, output: rx })
}

fn pump(mut reader: std::io::PipeReader, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

pub fn signal_group(pgid: i32, sig: Signal) -> bool {
    pgid > 0 && killpg(Pid::from_raw(pgid), sig).is_ok()
}

pub fn group_alive(pgid: i32) -> bool {
    pgid > 0 && killpg(Pid::from_raw(pgid), None).is_ok()
}

/// `SIGTERM` to the whole group, then `SIGKILL` after the grace period.
/// Returns the child's exit code (`None` when killed by a signal).
pub async fn terminate(child: &mut Child, pgid: i32) -> Option<i32> {
    signal_group(pgid, Signal::SIGTERM);
    let status = match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(s) => s.ok(),
        Err(_) => {
            signal_group(pgid, Signal::SIGKILL);
            child.wait().await.ok()
        }
    };
    // Members that ignored SIGTERM must not outlive the stop.
    if group_alive(pgid) {
        signal_group(pgid, Signal::SIGKILL);
    }
    status.and_then(|s| s.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect(mut s: Spawned) -> (String, Option<i32>) {
        let mut out = Vec::new();
        while let Some(c) = s.output.recv().await {
            out.extend(c);
        }
        let code = s.child.wait().await.unwrap().code();
        (String::from_utf8(out).unwrap(), code)
    }

    #[tokio::test]
    async fn merges_streams_in_order() {
        let env = BTreeMap::from([("GREETING".to_string(), "hi".to_string())]);
        let s = spawn_shell("echo $GREETING; echo err >&2; echo out; exit 3", Path::new("/"), &env).unwrap();
        let (out, code) = collect(s).await;
        assert_eq!(out, "hi\nerr\nout\n");
        assert_eq!(code, Some(3));
    }

    #[tokio::test]
    async fn stdin_is_dev_null() {
        let s = spawn_shell("read x; echo \"got[$x] $?\"", Path::new("/"), &BTreeMap::new()).unwrap();
        let (out, _) = collect(s).await;
        assert_eq!(out, "got[] 1\n");
    }

    #[tokio::test]
    async fn no_controlling_terminal() {
        let s =
            spawn_shell("if (exec 3</dev/tty) 2>/dev/null; then echo tty; else echo none; fi", Path::new("/"), &BTreeMap::new()).unwrap();
        let pid = s.pid;
        let (out, _) = collect(s).await;
        assert_eq!(out, "none\n");
        assert!(pid > 0);
    }

    #[tokio::test]
    async fn terminate_kills_the_group() {
        let mut s = spawn_shell("sleep 30 & sleep 30; wait", Path::new("/"), &BTreeMap::new()).unwrap();
        let pgid = s.pid;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(group_alive(pgid));
        terminate(&mut s.child, pgid).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Orphans may linger as zombies where PID 1 does not reap; count live members only.
        let ps = std::process::Command::new("ps").args(["-eo", "pgid=,stat="]).output().unwrap();
        let live = String::from_utf8_lossy(&ps.stdout)
            .lines()
            .filter(|l| {
                let mut it = l.split_whitespace();
                it.next() == Some(&pgid.to_string()) && !it.next().unwrap_or("").starts_with('Z')
            })
            .count();
        assert_eq!(live, 0);
    }
}
