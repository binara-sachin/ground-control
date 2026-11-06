//! What the TUI and the workers share: the tmux server, the
//! `groundcontrol` binary the windows run, the state folder, the config file
//! and a connection to the store. Passed explicitly, so tests can run side by
//! side with their own state folder and tmux server.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::store::Store;
use crate::tmux::Tmux;

pub struct Ctx {
    pub tmux: Tmux,
    /// The `groundcontrol` binary the windows run.
    pub exe: PathBuf,
    pub state_dir: PathBuf,
    pub config_path: PathBuf,
    store: Mutex<Store>,
}

impl Ctx {
    pub fn new(tmux: Tmux, exe: PathBuf, state_dir: PathBuf, config_path: PathBuf) -> Result<Ctx, String> {
        // Workers start in the project folder, so relative paths would resolve wrongly there.
        let state_dir = std::path::absolute(&state_dir).map_err(|e| e.to_string())?;
        // Resolved through symlinks, so the watcher sees edits to the real file.
        let config_path = match std::fs::canonicalize(&config_path) {
            Ok(p) => p,
            Err(_) => std::path::absolute(&config_path).map_err(|e| e.to_string())?,
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&state_dir)
            .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
        let store = Store::open(&state_dir.join(crate::paths::DB_FILE)).map_err(|e| format!("cannot open the state database: {e}"))?;
        Ok(Ctx { tmux, exe, state_dir, config_path, store: Mutex::new(store) })
    }

    /// The user's tmux server (inside a tmux pane: the pane's own server,
    /// through `$TMUX`), this binary, and the usual state and config paths.
    pub fn from_env() -> Result<Ctx, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot find the groundcontrol binary: {e}"))?;
        Ctx::new(Tmux::default_server(), exe, crate::paths::state_dir(), crate::paths::config_path())
    }

    /// Runs `f` against the store. The connection is never held across an `await`.
    pub fn db<T>(&self, f: impl FnOnce(&Store) -> rusqlite::Result<T>) -> Result<T, String> {
        f(&self.store.lock().unwrap()).map_err(|e| format!("state database: {e}"))
    }

    /// Variables every window gets, so its worker finds the same state and config.
    pub fn window_env(&self) -> Vec<(String, String)> {
        vec![
            ("GC_STATE_DIR".into(), self.state_dir.to_string_lossy().into_owned()),
            ("GC_CONFIG".into(), self.config_path.to_string_lossy().into_owned()),
        ]
    }
}
