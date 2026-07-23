//! Wire protocol for streaming AccessKit trees between machines.
//!
//! Message schema, DVC-compatible framing (messages are reassembled from
//! chunks sized to dynamic virtual channel limits), and the versioned
//! handshake with codec negotiation. Contains no I/O; byte transports live in
//! `accesskit_remote_transport`, and [`Session`] is the sans-I/O state
//! machine both ends drive.

pub use accesskit;

mod codec;
mod framing;
mod messages;
mod session;

pub use codec::{Codec, CodecError};
pub use framing::{frame, frame_into, FrameError, FrameReader, DEFAULT_MAX_FRAME_LEN, FRAME_HEADER_LEN};
pub use messages::{AppInfo, Hello, Message, PeerRole, WindowId};
pub use session::{Session, SessionConfig, SessionError, SessionEvent};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MIN_PROTOCOL_VERSION: u32 = 1;
