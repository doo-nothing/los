//! Los — a modular groovebox that lives in your terminal (see DESIGN.md).
//!
//! The crate is organized into four groups:
//!
//! - [`modules`] — the runnable pane processes (sequencer, voice, mixer, …).
//!   Start here to contribute a new module.
//! - [`ui`] — shared TUI components (theme, vi keys, `:` command line, …).
//! - [`ipc`] — shared-memory plumbing between module processes.
//! - [`session`] — persistence and the tmux rack.
//!
//! Every module is also re-exported at the crate root (`los::sequencer`,
//! `crate::shm`, …) so call sites stay short and moving a file between
//! groups never breaks imports.

pub mod ipc;
pub mod modules;
pub mod session;
pub mod ui;

pub use ipc::{routing, shm};
pub use modules::{badge, conductor, envelope, mixer, scope, sequencer, tone, voice};
pub use session::{layout, state, tmux};
pub use ui::{excmd, keys, picker, theme, undo};

pub const NUM_TRACKS: usize = 8;
