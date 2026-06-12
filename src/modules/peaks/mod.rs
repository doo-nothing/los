//! # Peaks — the Mutable Instruments dual function generator, as a
//! los module
//!
//! Full fixed-point port of peaks (MIT, copyright 2013 Emilie
//! Gillet): the 808 drum models, envelopes, LFOs, tap LFO, pulse
//! processors, bouncing ball, mini sequencer, and the number-station
//! easter egg. `dsp.rs` holds the drum engines and tables;
//! `mods.rs` the modulation/pulse engines; `ui.rs` the two-channel
//! module shell.

pub mod dsp;
pub mod mods;
pub mod ui;

pub use ui::run;
