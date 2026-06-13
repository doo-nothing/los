//! # Marbles — the Mutable Instruments random sampler
//!
//! Full port of marbles' random-generation core (MIT, copyright 2015
//! Emilie Gillet): the déjà-vu loop, the Beta-distribution voltage
//! shaping with the real scipy ICDF tables, the output-channel
//! smooth/quantized morph, and the t-generator rhythm models. The
//! ramp/clock infrastructure is replaced by the los transport clock.
//! `dsp.rs` is the engine; `ui.rs` the shell publishing
//! marbles/N/{t1,t2,t3,x1,x2,x3,y}.

pub mod dsp;
pub mod ui;

pub use ui::run;
