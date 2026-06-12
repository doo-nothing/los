//! Session plumbing: persistence and the tmux "rack".
//!
//! [`state`] owns the on-disk layout (saved sessions, per-module tmp state)
//! and serialization, [`layout`] reads the house pane layout from
//! `los.toml`, [`tmux`] is the thin wrapper around the `tmux` binary
//! that the conductor drives, and [`validate`] checks state files offline
//! (`los check`, and the gate `los load` runs first).

pub mod layout;
pub mod state;
pub mod tmux;
pub mod validate;
