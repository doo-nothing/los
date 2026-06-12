//! Session plumbing: persistence and the tmux "rack".
//!
//! [`state`] owns the on-disk layout (saved sessions, per-module tmp state)
//! and serialization, [`layout`] reads the house pane layout from
//! `los.toml`, [`tmux`] is the thin wrapper around the `tmux` binary
//! that the conductor drives, [`validate`] checks state files offline
//! (`los check`, and the gate `los load` runs first), and [`song`] walks
//! a sequencer's macro lane into absolute time (`los audit --song`).

pub mod layout;
pub mod song;
pub mod state;
pub mod tmux;
pub mod validate;
