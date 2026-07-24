//! AT-SPI tree source (Linux).
//!
//! Consumes the AT-SPI accessibility bus like a screen reader would and
//! mirrors each toplevel frame into an AccessKit tree, emitting incremental
//! tree updates. AT-SPI events are treated as invalidation hints, not truth:
//! structural events trigger subtree re-walks diffed against the mirror, and
//! value/state events are re-read live before being applied.
#![cfg(target_os = "linux")]

mod coalesce;
mod focus;
pub mod mapping;
mod mirror;
pub mod reconcile;
mod source;

pub use source::AtspiSource;
