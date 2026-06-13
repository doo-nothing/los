//! # Warps — the Mutable Instruments meta-modulator
//!
//! Full port of warps' cross-modulation core (MIT, copyright 2014
//! Emilie Gillet): the six cross-mod algorithms with the morph, the
//! saturating amplifier, the internal carrier oscillator, and a
//! channel vocoder. `dsp.rs` is the engine; `ui.rs` the FX shell
//! claiming two inputs (carrier + modulator) and publishing the
//! processed audio plus warps/N/aux.

pub mod dsp;
pub mod ui;

pub use ui::run;
