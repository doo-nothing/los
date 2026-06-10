//! Shared TUI components used by every module: the design language
//! ([`theme`]), vi-style key handling ([`keys`]), the `:` command line
//! ([`excmd`]), the `@` source picker ([`picker`]), and param undo/redo
//! ([`undo`]).
//!
//! Anything visual or interactive that two or more modules need belongs
//! here, so the panes stay consistent (docs/plans/design-language.md).

pub mod excmd;
pub mod keys;
pub mod picker;
pub mod theme;
pub mod undo;
