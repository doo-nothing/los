//! # Clouds — the Mutable Instruments granular processor
//!
//! Port of clouds (MIT, copyright 2014 Emilie Gillet). `dsp.rs` holds
//! the granular core (circular buffer, windowed overlap-add grains,
//! the density/position/size/pitch scheduler). The reverb stage and
//! the FX shell build on it.

pub mod dsp;
pub mod ui;

pub use ui::run;
