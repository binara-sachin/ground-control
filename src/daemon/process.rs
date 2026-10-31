//! Spawning shell commands in their own process group with stdout and stderr
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

/// Runs `/bin/zsh -l -c <command>` in `dir`, stdin from `/dev/null`, in a new
/// process group whose id equals the child's pid.
pub fn spawn_shell(command: &str, dir: &Path, env: &BTreeMap<String, String>) -> std::io::Result<Spawned> {
    let (reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new(SHELL);
    cmd.arg("-l")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .stderr(Stdio::from(writer))
        .process_group(0)
        .kill_on_drop(false);
    let child = cmd.spawn()?;
    // Dropping `cmd` closes our copies of the write end, so EOF arrives once
    // the child (and anything it left running) has closed its copies.
    drop(cmd);
    let pid = child.id().map(|p| p as i32).unwrap_or(-1);
    let (tx, rx) = mpsc::channel(256);
    std::thread::Builder::new()
        .name(format!("gc-output-{pid}"))
        .spawn(move || pump(reader, tx))?;
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

/// Terminates a group we no longer hold a `Child` handle for (left over from a
/// previous daemon process).
pub async fn terminate_orphan_group(pgid: i32) {
    if !group_alive(pgid) {
        return;
    }
    signal_group(pgid, Signal::SIGTERM);
    let deadline = tokio::time::Instant::now() + KILL_GRACE;
    while group_alive(pgid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if group_alive(pgid) {
        signal_group(pgid, Signal::SIGKILL);
    }
}

/// Runs a short command through the login shell and returns trimmed stdout.
pub async fn shell_output(command: &str, timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(SHELL);
    cmd.arg("-l").arg("-c").arg(command).stdin(Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => Err("timed out".into()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let msg = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            Err(if msg.is_empty() { format!("exit {}", out.status.code().unwrap_or(-1)) } else { msg.to_string() })
        }
    }
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
