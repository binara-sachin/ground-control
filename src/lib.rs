//! GroundControl: a personal local developer platform (see `docs/groundcontrol-spec.md`).
//!
//! One binary. `groundcontrol` opens the TUI. `groundcontrol svc-exec` and
//! `groundcontrol run-exec` are the commands of the tmux windows that run
//! services and pipeline runs (docs/redesign-daemonless.md). There is no
//! daemon and nothing starts at login.

pub mod config;
pub mod ctx;
pub mod engine;
pub mod envfile;
pub mod git;
pub mod lock;
pub mod logfile;
pub mod mask;
pub mod paths;
pub mod process;
pub mod protocol;
pub mod runs;
pub mod services;
pub mod store;
pub mod tmux;
pub mod tui;
pub mod yaml;
