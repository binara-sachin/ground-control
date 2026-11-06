//! Advisory `flock` locks that mark a run or a service as owned by a live
//! worker process (docs/redesign-daemonless.md, "Locking and concurrency").
//!
//! The kernel releases a lock when the process holding it exits, however it
//! exits, so a lock can never go stale and no PID is ever checked or signalled.
//! Locks belong to the open file, so two `try_acquire` calls on the same path
//! conflict even inside one process.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

/// A held lock. Dropping it releases the lock; the file itself is kept.
#[derive(Debug)]
pub struct Lock {
    _file: Flock<File>,
    path: PathBuf,
}

impl Lock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Takes the lock at `path` without blocking, creating the file (`0600`) and
/// its folders (`0700`) as needed. `Ok(None)` means another holder has it.
pub fn try_acquire(path: &Path) -> io::Result<Option<Lock>> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(f) => Ok(Some(Lock { _file: f, path: path.to_path_buf() })),
        Err((_, Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, e)) => Err(e.into()),
    }
}

/// `try_acquire`, retried for up to ~160 ms. `is_held` probes take the lock
/// for a moment, so a probe from a polling TUI must not make a start fail.
pub fn acquire(path: &Path) -> io::Result<Option<Lock>> {
    for attempt in 0..5 {
        if let Some(l) = try_acquire(path)? {
            return Ok(Some(l));
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    Ok(None)
}

/// Whether some process holds the lock at `path`. A missing file is not held.
///
/// The probe briefly takes the lock itself when it is free, so it must not be
/// used to decide whether to start something: use `try_acquire` for that.
pub fn is_held(path: &Path) -> io::Result<bool> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_) => Ok(false),
        Err((_, Errno::EWOULDBLOCK)) => Ok(true),
        Err((_, e)) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn second_acquire_fails_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p/run.lock");
        assert!(!is_held(&path).unwrap());

        let lock = try_acquire(&path).unwrap().expect("first acquire");
        assert_eq!(lock.path(), path);
        assert!(try_acquire(&path).unwrap().is_none());
        assert!(is_held(&path).unwrap());

        drop(lock);
        assert!(!is_held(&path).unwrap());
        assert!(try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn probe_does_not_keep_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        drop(try_acquire(&path).unwrap());
        assert!(!is_held(&path).unwrap());
        assert!(try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks/p/services/api.lock");
        let _lock = try_acquire(&path).unwrap().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn released_when_the_holder_exits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        std::fs::write(&path, "").unwrap();
        // `flock(1)` is not on macOS, so a child process holds the lock through
        // Perl's flock, which is the same system call.
        let mut child = std::process::Command::new("perl")
            .args([
                "-e",
                "open(my $f, '<', $ARGV[0]) or die; flock($f, 2) or die; print \"ok\\n\"; $| = 1; sleep 30",
                path.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.as_mut().unwrap()), &mut line).unwrap();
        assert_eq!(line, "ok\n");
        assert!(is_held(&path).unwrap());
        assert!(try_acquire(&path).unwrap().is_none());

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_held(&path).unwrap());
    }
}
