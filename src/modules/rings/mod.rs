//! # Rings — the Mutable Instruments resonator, as a los module
//!
//! Full port of rings' DSP (modal resonator, sympathetic strings,
//! inharmonic string, FM voice, string-and-reverb), MIT, copyright
//! 2015 Emilie Gillet. `dsp.rs` holds the primitives and lookup
//! laws, `models.rs` the resonator/string/FM voices, `part.rs` the
//! polyphonic dispatcher, `ui.rs` the module shell.

pub mod dsp;
pub mod models;
pub mod part;
pub mod string_synth;
pub mod ui;

pub use ui::run;
