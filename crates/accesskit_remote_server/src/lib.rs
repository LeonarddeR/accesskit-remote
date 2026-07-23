//! Server core: session/window registry, tree multiplexing, action routing.
//!
//! Consumes a `TreeSource` (window lifecycle and tree updates in, action
//! requests out) and serves connected clients over a transport. Source- and
//! transport-agnostic; the AT-SPI source lives in `accesskit_remote_atspi`.
