//! Session plumbing: persistence and the tmux "rack".
//!
//! [`state`] owns the on-disk layout (saved sessions, per-module tmp state)
//! and serialization, [`layout`] reads the house pane layout from
//! `los.toml`, and [`tmux`] is the thin wrapper around the `tmux` binary
//! that the conductor drives.

pub mod layout;
pub mod state;
pub mod tmux;
