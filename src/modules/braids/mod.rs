//! # Braids — the Mutable Instruments macro-oscillator
//!
//! Port of braids (MIT, copyright 2014 Emilie Gillet). `dsp.rs` holds
//! the analog oscillator (nine band-limited shapes with sync); the
//! macro-oscillator model wiring and the digital models build on it.

pub mod dsp;
