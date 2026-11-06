//! What the TUI and the workers share: the tmux server, the `groundcontrol`
//! binary the windows run, and the config file. Passed explicitly, so tests
//! can run side by side with their own tmux server and config.

use std::path::PathBuf;

use crate::tmux::Tmux;

pub struct Ctx {
    pub tmux: Tmux,
    /// The `groundcontrol` binary the windows run.
    pub exe: PathBuf,
    pub config_path: PathBuf,
}

impl Ctx {
    pub fn new(tmux: Tmux, exe: PathBuf, config_path: PathBuf) -> Result<Ctx, String> {
        // Resolved through symlinks, so the watcher sees edits to the real
        // file; absolute, because workers start in the project folder.
        let config_path = match std::fs::canonicalize(&config_path) {
            Ok(p) => p,
            Err(_) => std::path::absolute(&config_path).map_err(|e| e.to_string())?,
        };
        Ok(Ctx { tmux, exe, config_path })
    }

    /// The user's tmux server (inside a tmux pane: the pane's own server,
    /// through `$TMUX`), this binary, and the usual config path.
    pub fn from_env() -> Result<Ctx, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot find the groundcontrol binary: {e}"))?;
        Ctx::new(Tmux::default_server(), exe, crate::paths::config_path())
    }

    /// Variables every window gets, so its worker reads the same config.
    pub fn window_env(&self) -> Vec<(String, String)> {
        vec![("GC_CONFIG".into(), self.config_path.to_string_lossy().into_owned())]
    }
}
