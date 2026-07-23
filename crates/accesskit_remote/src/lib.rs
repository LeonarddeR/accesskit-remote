//! Wire protocol for streaming AccessKit trees between machines.
//!
//! Message schema, DVC-compatible framing (messages are reassembled from
//! chunks sized to dynamic virtual channel limits), and the versioned
//! handshake with codec negotiation. Contains no I/O; byte transports live in
//! `accesskit_remote_transport`.
