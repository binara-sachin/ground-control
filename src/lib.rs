//! GroundControl: a personal local developer platform (see `docs/groundcontrol-spec.md`).
//!
//! One binary, two modes: `groundcontrol daemon` (the background daemon, run by
//! `launchd`) and `groundcontrol` (the TUI client).

pub mod client;
pub mod config;
pub mod daemon;
pub mod envfile;
pub mod git;
pub mod mask;
pub mod paths;
pub mod protocol;
pub mod tui;
pub mod yaml;
