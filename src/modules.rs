//! The runnable modules — each one is a separate process living in its own
//! tmux pane, launched via `los <module> [instance]`.
//!
//! To contribute a new module, read [`template`] — a small, fully wired,
//! heavily commented worked example written to be copied
//! (docs/writing-a-module.md is the matching guide; docs/writing-dsp.md
//! covers writing the audio-rate core in a DSP language). Then wire yours
//! up in three places:
//!
//! 1. Declare it here and re-export it from `lib.rs`.
//! 2. Add it to `dispatch_module` in `main.rs`.
//! 3. Register it in `conductor` (`canonical_module`, and
//!    `ADDABLE_MODULES` if it can be spawned at runtime with `los add`).
//!
//! Modules talk to the rest of the rig exclusively through [`crate::ipc`]
//! (audio rings, event ring, modbus, transport clock) and draw with the
//! shared components in [`crate::ui`]. The editing contract every
//! instrument module follows lives in docs/keybindings.md.

pub mod badge;
pub mod branches;
pub mod conductor;
pub mod delay;
pub mod dld;
pub mod dpo;
pub mod edges;
pub mod elements;
pub mod envelope;
pub mod filterbank;
pub mod frames;
pub mod grids;
pub mod lfo;
pub mod marbles;
pub mod mixer;
pub mod peaks;
pub mod rings;
pub mod sampler;
pub mod scope;
pub mod wasp;
pub mod sequencer;
pub mod stages;
pub mod streams;
pub mod tape;
pub mod tides;
pub mod swarm;
pub mod template;
pub mod tone;
pub mod voice;
