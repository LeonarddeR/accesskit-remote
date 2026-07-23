//! Client core: receives the protocol, maintains per-window tree stores, and
//! routes actions back to the server.
//!
//! Exposes windows, trees, and focus through a host trait; how trees are
//! surfaced to a platform accessibility API (and how remote windows are
//! associated with local ones) is the host's concern — see
//! `accesskit_remote_windows` for the UIA host.
