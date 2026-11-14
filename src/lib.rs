//! GroundControl: a personal local developer platform (see `docs/groundcontrol-spec.md`).
//!
//! One binary. `groundcontrol` opens the TUI. `groundcontrol svc-exec` and
//! `groundcontrol run-exec` are the commands of the tmux windows that run
//! services and pipeline runs (docs/architecture.md). Nothing runs in the
//! background on its own and nothing starts at login.

pub mod config;
pub mod ctx;
pub mod devscan;
pub mod engine;
pub mod envfile;
pub mod envlayers;
pub mod git;
pub mod mask;
pub mod model;
pub mod paths;
pub mod process;
pub mod runs;
pub mod services;
pub mod tmux;
pub mod tui;
pub mod window;
pub mod worker;
pub mod yaml;
