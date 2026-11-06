//! Where GroundControl finds its config. It stores nothing else on disk.

use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "groundcontrol.yaml";

/// `~/.config/ground-control`, where `make install` links the repository's
/// `groundcontrol.yaml` so the TUI finds it from any folder.
pub fn config_dir() -> PathBuf {
    home_dir().join(".config").join("ground-control")
}

/// Location of `groundcontrol.yaml`, in this order:
///
/// 1. `GC_CONFIG` (the TUI passes it on to every worker window);
/// 2. `~/.config/ground-control/groundcontrol.yaml`, the link `make install` writes;
/// 3. `groundcontrol.yaml` in the current directory.
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GC_CONFIG") {
        return expand_tilde(&PathBuf::from(p));
    }
    let installed = config_dir().join(CONFIG_FILE);
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
