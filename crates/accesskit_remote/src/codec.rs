//! Message encoding. JSON is the handshake codec and the only codec in
//! protocol version 1; additional codecs negotiate through the handshake.

use crate::messages::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Json,
}

impl Codec {
    pub const HANDSHAKE: Codec = Codec::Json;

    pub fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    pub fn encode(self, msg: &Message) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::Json => serde_json::to_vec(msg).map_err(|e| CodecError(e.to_string())),
        }
    }

    pub fn decode(self, bytes: &[u8]) -> Result<Message, CodecError> {
        match self {
            Self::Json => serde_json::from_slice(bytes).map_err(|e| CodecError(e.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecError(pub String);

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "codec error: {}", self.0)
    }
}

impl std::error::Error for CodecError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip() {
        let msg = Message::Ping { seq: 5 };
        let bytes = Codec::Json.encode(&msg).unwrap();
        match Codec::Json.decode(&bytes).unwrap() {
            Message::Ping { seq } => assert_eq!(seq, 5),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn decode_error_on_garbage() {
        assert!(Codec::Json.decode(b"not json").is_err());
    }

    #[test]
    fn name_round_trip() {
        assert_eq!(Codec::from_name(Codec::Json.name()), Some(Codec::Json));
        assert_eq!(Codec::from_name("bogus"), None);
    }
}
