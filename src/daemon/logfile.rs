//! Append-only log files with size-based rotation (service logs and the
//! daemon's own log: 10 MB, 3 files kept).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
pub const KEEP_FILES: usize = 3;

pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_limits(path, ROTATE_BYTES, KEEP_FILES)
    }

    pub fn with_limits(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> Self {
        RotatingFile { path: path.into(), max_bytes, keep, file: None, size: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            let f = OpenOptions::new().create(true).append(true).open(&self.path)?;
            self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(f);
        }
        Ok(self.file.as_mut().expect("just opened"))
    }

    /// Shifts `log` → `log.1` → … → `log.<keep-1>`, dropping the oldest.
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let name = |i: usize| -> PathBuf {
            let mut p = self.path.clone().into_os_string();
            p.push(format!(".{i}"));
            PathBuf::from(p)
        };
        if self.keep > 1 {
            let _ = fs::remove_file(name(self.keep - 1));
            for i in (1..self.keep - 1).rev() {
                let _ = fs::rename(name(i), name(i + 1));
            }
            let _ = fs::rename(&self.path, name(1));
        } else {
            let _ = fs::remove_file(&self.path);
        }
        self.size = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let f = self.open()?;
        f.write_all(buf)?;
        self.size += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Reads up to `max_bytes` from the end of a file, starting at a line boundary.
pub fn tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    if start > 0
        && let Some(nl) = buf.iter().position(|&b| b == b'\n')
    {
        buf.drain(..=nl);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_keeps_n_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.log");
        let mut f = RotatingFile::with_limits(&path, 10, 3);
        for i in 0..10 {
            writeln!(f, "line{i:04}").unwrap();
        }
        assert!(path.exists());
        assert!(dir.path().join("svc.log.1").exists());
        assert!(dir.path().join("svc.log.2").exists());
        assert!(!dir.path().join("svc.log.3").exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "line0009\n");
        assert_eq!(fs::read_to_string(dir.path().join("svc.log.1")).unwrap(), "line0008\n");
    }

    #[test]
    fn tail_starts_on_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.log");
        fs::write(&path, "first line\nsecond line\nthird\n").unwrap();
        assert_eq!(tail(&path, 15).unwrap(), "third\n");
        assert_eq!(tail(&path, 1000).unwrap(), "first line\nsecond line\nthird\n");
    }
}
