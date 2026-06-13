//! # Frames — the Mutable Instruments keyframer, as a los module
//!
//! Full port of frames (MIT, copyright 2013 Emilie Gillet): the
//! keyframe store with easing curves and 2164 response blending,
//! plus the poly-LFO easter egg. `dsp.rs` is the engine, `ui.rs`
//! the shell. Outputs frames/N/ch1-ch4 on the modulation bus; the
//! frame knob itself takes a cable — that's the whole instrument.

pub mod dsp;
