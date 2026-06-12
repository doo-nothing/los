//! # Tides — the Mutable Instruments tidal modulator (2018), as a
//! los module
//!
//! Full port of tides2's PolySlopeGenerator (MIT, copyright 2017
//! Emilie Gillet): AD / looping / AR ramps, four output modes,
//! control + audio ranges, the polyrhythmic ratio tables, and the
//! transport-locked external-ramp path. `dsp.rs` is the engine,
//! `ui.rs` the module shell.

pub mod dsp;
