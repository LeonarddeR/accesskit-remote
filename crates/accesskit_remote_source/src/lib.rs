//! Building blocks shared by every [`TreeSource`] implementation, independent
//! of the accessibility API being mirrored.
//!
//! A tree source turns a live accessibility API into AccessKit trees. The parts
//! that are genuinely about *that* API — role maps, state distillation, the
//! reads themselves — belong in the per-API crate. What is left over is
//! bookkeeping every source needs and none of them should re-derive:
//!
//! - [`coalesce::RewalkCoalescer`] — trailing debounce collapsing a burst of
//!   structural invalidations into one re-walk per window.
//! - [`limiter::NodeRefreshLimiter`] — leading-plus-trailing rate limit on
//!   per-node semantic refreshes.
//! - [`focus::FocusTracker`] — deduplicated window-level focus bookkeeping.
//! - [`reconcile::reconcile_windows`] — diffing a tracked window set against a
//!   fresh discovery.
//!
//! Everything here is pure and clock-free: callers supply `now`, so the whole
//! crate is unit tested with no accessibility API present at all, on every
//! platform rather than only the one its source targets.
//!
//! [`TreeSource`]: https://docs.rs/accesskit_remote_server

pub mod coalesce;
pub mod focus;
pub mod limiter;
pub mod reconcile;
