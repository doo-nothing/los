//! # Stages — the Mutable Instruments segment generator
//!
//! Full port of stages (MIT, copyright 2017 Emilie Gillet): six
//! segments wired into groups by gate bindings, each group a
//! multi-stage envelope, LFO, sequencer, S&H, pulse, delay or
//! oscillator depending on its panel configuration — exactly the
//! firmware's Configure law. `dsp.rs` is the engine; `ui.rs` the
//! six-segment shell publishing stages/N/o1..o6.

pub mod dsp;
