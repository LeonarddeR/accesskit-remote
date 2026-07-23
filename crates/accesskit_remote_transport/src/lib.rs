//! Byte transports for the AccessKit remote protocol.
//!
//! Streams carry opaque, ordered bytes between the provider and the
//! consumer; framing, handshake, and messages live in `accesskit_remote`,
//! so every transport here is a plain constructor for a connected or
//! listening socket. An RDP dynamic virtual channel replaces these by
//! feeding its chunks to the same sans-I/O session.
//!
//! Available transports:
//! - TCP on localhost (development fallback, any platform)
//! - `AF_VSOCK` listener (Linux guest side, e.g. a WSL user distro)
//! - `AF_HYPERV` connector (Windows host side, e.g. inside the RDP client)

pub mod tcp;

#[cfg(target_os = "linux")]
pub mod vsock;

#[cfg(windows)]
pub mod hvsocket;

pub mod hvsocket_addr;

pub use socket2::Socket;
