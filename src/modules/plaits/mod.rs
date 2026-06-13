//! # Plaits — the Mutable Instruments macro-oscillator (the big one)
//!
//! Port of plaits (MIT, copyright 2016 Emilie Gillet): a bank of ~20
//! synthesis engines behind three macro knobs. `dsp.rs` holds the
//! shared scaffold and the engines, ported in stages; the voice shell
//! drives them from a note source. This is the largest catalog
//! module — engines land incrementally.

pub mod dsp;
pub mod ui;

pub use ui::run;
