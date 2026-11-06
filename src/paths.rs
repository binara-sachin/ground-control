//! Filesystem locations used by the TUI and the workers (§11).

use std::path::{Path, PathBuf};

/// Folder name used inside sensitive repositories.
pub const REPO_STATE_DIR: &str = ".groundcontrol";

/// The state folder: `~/Library/Application Support/GroundControl`.
///
/// `GC_STATE_DIR` overrides it, which the tests and local development use.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GC_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/GroundControl")
    } else {
        dirs::data_dir().unwrap_or_else(|| home_dir().join(".local/share")).join("GroundControl")
    }
}

/// File name of the SQLite store inside the state folder.
pub const DB_FILE: &str = "state.db";

pub const CONFIG_FILE: &str = "groundcontrol.yaml";

/// Location of `groundcontrol.yaml`, in this order:
///
/// 1. `GC_CONFIG` (the TUI passes it on to every worker window);
/// 2. `<state>/groundcontrol.yaml`, the symlink `make install` points at the
///    repository's file, so the TUI finds it from any folder;
/// 3. `groundcontrol.yaml` in the current directory.
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GC_CONFIG") {
        return expand_tilde(&PathBuf::from(p));
    }
    let installed = state_dir().join(CONFIG_FILE);
    if installed.exists() {
        return installed;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(CONFIG_FILE)
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~` or `~/`.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        home_dir()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        p.to_path_buf()
    }
}

/// Root folder for a project's logs and diffs: central or in-repo (sensitive).
pub fn project_data_root(project_path: &Path, sensitive: bool) -> PathBuf {
    if sensitive { project_path.join(REPO_STATE_DIR) } else { state_dir() }
}

/// `logs/<run-id>/<step>.log`
pub fn step_log_path(root: &Path, run_id: i64, step: usize) -> PathBuf {
    root.join("logs").join(run_id.to_string()).join(format!("{step}.log"))
}

/// `diffs/<run-id>.diff`
pub fn diff_path(root: &Path, run_id: i64) -> PathBuf {
    root.join("diffs").join(format!("{run_id}.diff"))
}

/// `services/<project>/<service>.log` centrally, `services/<service>.log` in-repo.
pub fn service_log_path(project: &str, project_path: &Path, sensitive: bool, service: &str) -> PathBuf {
    if sensitive {
        project_path.join(REPO_STATE_DIR).join("services").join(format!("{service}.log"))
    } else {
        state_dir().join("services").join(project).join(format!("{service}.log"))
    }
}

/// `<state>/locks/<project>/run.lock`: held by a project's active run for its lifetime.
pub fn run_lock_path(state: &Path, project: &str) -> PathBuf {
    state.join("locks").join(project).join("run.lock")
}

/// `<state>/locks/<project>/services/<service>.lock`: held by the service's worker while it is up.
pub fn service_lock_path(state: &Path, project: &str, service: &str) -> PathBuf {
    state.join("locks").join(project).join("services").join(format!("{service}.lock"))
}

/// Run-scoped scratch files (`$GC_OUTPUT`). Never in a repository.
pub fn run_tmp_dir(run_id: i64) -> PathBuf {
    state_dir().join("tmp").join(run_id.to_string())
}
