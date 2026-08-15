//! Reading the on-disk demo format (HL2DEMO, demo protocol 3).
//!
//! This layer knows how NT;RE `.dem` files are laid out and nothing about
//! gameplay: it exposes the fixed header and an iterator over raw frames.
//! Anything that interprets frame *contents* belongs in `crate::extract`.

pub mod bits;
pub mod frames;
pub mod header;
pub mod usercmd;
