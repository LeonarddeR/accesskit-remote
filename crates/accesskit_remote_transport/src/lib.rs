//! Byte transports with DVC semantics.
//!
//! A transport moves opaque, ordered chunks between the two ends of a
//! connection, mirroring the lifecycle of an RDP dynamic virtual channel
//! (open, chunked receive, close) so a real DVC is a drop-in replacement for
//! the socket implementations (TCP, vsock/hvsocket, Unix socket).
