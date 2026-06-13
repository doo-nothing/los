// Los — a modular groovebox that lives in your terminal
// Copyright (C) 2026 doo-nothing / AU Supply
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version. See LICENSE.

//! Los — a modular groovebox that lives in your terminal (see DESIGN.md).
//!
//! The crate is organized into five groups:
//!
//! - [`modules`] — the runnable pane processes (sequencer, voice, mixer, …).
//!   Start here to contribute a new module.
//! - [`ui`] — shared TUI components (theme, vi keys, `:` command line, …).
//! - [`ipc`] — shared-memory plumbing between module processes.
//! - [`session`] — persistence and the tmux rack.
//! - [`theory`] — scales (microtonal included), Scala `.scl` import, and
//!   pattern generators.
//!
//! Every module is also re-exported at the crate root (`los::sequencer`,
//! `crate::shm`, …) so call sites stay short and moving a file between
//! groups never breaks imports.

pub mod audit;
pub mod faust;
pub mod ipc;
pub mod modules;
pub mod session;
pub mod theory;
pub mod ui;

pub use ipc::{routing, shm};
pub use modules::{
    badge, branches, conductor, delay, dld, dpo, edges, elements, envelope, filterbank, frames, grids, lfo, mixer, peaks, rings,
    sampler, tides,
    scope, marbles, sequencer, stages, streams, swarm, tape, wasp,
    template,
    tone, voice,
};
pub use session::{layout, song, state, tmux, validate};
pub use ui::{excmd, keys, picker, theme, undo};

pub const NUM_TRACKS: usize = 8;
