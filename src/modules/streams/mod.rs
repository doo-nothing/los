//! # Streams — the Mutable Instruments dual dynamics gate
//!
//! Full port of streams (MIT, copyright 2014 Emilie Gillet): the
//! envelope, vactrol, follower, compressor, filter controller, and
//! Lorenz generator, each mapping (audio, excite) to a VCA gain and
//! a filter-frequency CV. `dsp.rs` is the engines, `ui.rs` the
//! two-channel shell (audio input claim; gains applied in-line;
//! gain + frequency published as streams/N/{g1,f1,g2,f2}).

pub mod dsp;
