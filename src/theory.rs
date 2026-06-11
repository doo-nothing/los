//! Music theory engine shared by the instrument modules.
//!
//! [`scales`] is the cents-based scale engine and its built-in library
//! (microtonal included), [`scl`] parses Scala `.scl` tuning files into
//! scales, [`gen`] holds the pattern generators behind the sequencer's
//! auto-fill commands, [`groove`] is the micro-timing template library
//! behind `:groove`, and [`rng`] is the tiny deterministic PRNG they
//! share (the crate deliberately has no `rand` dependency).

pub mod gen;
pub mod groove;
pub mod rng;
pub mod scales;
pub mod scl;
