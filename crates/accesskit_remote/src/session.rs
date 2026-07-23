//! Sans-I/O session state machine.
//!
//! A [`Session`] owns one end of a connection: it produces the outgoing byte
//! stream (handshake, encoded messages) and consumes the incoming one,
//! yielding [`SessionEvent`]s. The caller moves bytes between the session
//! and whatever transport carries them.
//!
//! Handshake: both peers send a [`Hello`] immediately. The session version
//! is the minimum of both `version` fields; the codec is the first entry in
//! the provider's list the consumer also supports. Pings are answered
//! automatically; incoming pongs are surfaced for RTT measurement.

use crate::codec::{Codec, CodecError};
use crate::framing::{frame_into, FrameError, FrameReader, DEFAULT_MAX_FRAME_LEN};
use crate::messages::{Hello, Message, PeerRole};
use crate::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub role: PeerRole,
    pub name: String,
    pub codecs: Vec<Codec>,
    pub max_frame_len: usize,
}

impl SessionConfig {
    pub fn new(role: PeerRole, name: impl Into<String>) -> Self {
        Self {
            role,
            name: name.into(),
            codecs: vec![Codec::Json],
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
        }
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    Established { version: u32, codec: Codec },
    Message(Message),
    Closed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    Frame(FrameError),
    Codec(CodecError),
    UnexpectedMessage(String),
    RoleConflict,
    IncompatibleVersion { ours: u32, theirs: u32 },
    NoCommonCodec,
    NotEstablished,
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Frame(e) => write!(f, "{e}"),
            Self::Codec(e) => write!(f, "{e}"),
            Self::UnexpectedMessage(what) => write!(f, "unexpected message: {what}"),
            Self::RoleConflict => write!(f, "both peers claim the same role"),
            Self::IncompatibleVersion { ours, theirs } => {
                write!(f, "incompatible protocol versions (ours {ours}, theirs {theirs})")
            }
            Self::NoCommonCodec => write!(f, "no common codec"),
            Self::NotEstablished => write!(f, "session not established"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<FrameError> for SessionError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

impl From<CodecError> for SessionError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

#[derive(Debug, Clone, Copy)]
enum State {
    AwaitingHello,
    Established { codec: Codec },
    Closed,
}

#[derive(Debug)]
pub struct Session {
    config: SessionConfig,
    reader: FrameReader,
    state: State,
    out: Vec<u8>,
}

impl Session {
    /// Creates a session with its outgoing `Hello` already queued; drain it
    /// with [`take_output`](Self::take_output) and send it to the peer.
    pub fn new(config: SessionConfig) -> Self {
        let hello = Message::Hello(Hello {
            version: PROTOCOL_VERSION,
            role: config.role,
            codecs: config.codecs.iter().map(|c| c.name().to_string()).collect(),
            name: config.name.clone(),
        });
        let reader = FrameReader::with_max_len(config.max_frame_len);
        let mut session = Self {
            config,
            reader,
            state: State::AwaitingHello,
            out: Vec::new(),
        };
        session
            .queue(Codec::HANDSHAKE, &hello)
            .expect("encoding our own hello cannot fail");
        session
    }

    pub fn is_established(&self) -> bool {
        matches!(self.state, State::Established { .. })
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed)
    }

    /// Drains bytes that must be sent to the peer.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    /// Consumes a chunk received from the transport.
    ///
    /// On a fatal error the session queues a `Goodbye`, closes, and returns
    /// the error; the caller should flush [`take_output`](Self::take_output)
    /// before dropping the transport.
    pub fn handle_input(&mut self, chunk: &[u8]) -> Result<Vec<SessionEvent>, SessionError> {
        if self.is_closed() {
            return Ok(Vec::new());
        }
        self.reader.push(chunk);
        let mut events = Vec::new();
        self.drain_frames(&mut events)
            .inspect_err(|e| self.close(e.to_string()))?;
        Ok(events)
    }

    fn drain_frames(&mut self, events: &mut Vec<SessionEvent>) -> Result<(), SessionError> {
        while let Some(payload) = self.reader.next_frame()? {
            match self.state {
                State::AwaitingHello => events.push(self.handle_hello(&payload)?),
                State::Established { codec } => match codec.decode(&payload)? {
                    Message::Hello(_) => {
                        return Err(SessionError::UnexpectedMessage(
                            "hello after handshake".into(),
                        ));
                    }
                    Message::Goodbye { reason } => {
                        self.state = State::Closed;
                        events.push(SessionEvent::Closed { reason });
                        break;
                    }
                    Message::Ping { seq } => self.queue(codec, &Message::Pong { seq })?,
                    msg => events.push(SessionEvent::Message(msg)),
                },
                State::Closed => break,
            }
        }
        Ok(())
    }

    /// Encodes and queues an application message.
    pub fn send(&mut self, msg: &Message) -> Result<(), SessionError> {
        let State::Established { codec } = self.state else {
            return Err(SessionError::NotEstablished);
        };
        if matches!(msg, Message::Hello(_) | Message::Goodbye { .. }) {
            return Err(SessionError::UnexpectedMessage(
                "hello and goodbye are managed by the session".into(),
            ));
        }
        self.queue(codec, msg)
    }

    /// Queues a `Goodbye` and closes the session.
    pub fn close(&mut self, reason: impl Into<String>) {
        if self.is_closed() {
            return;
        }
        let codec = match self.state {
            State::Established { codec } => codec,
            _ => Codec::HANDSHAKE,
        };
        let _ = self.queue(
            codec,
            &Message::Goodbye {
                reason: reason.into(),
            },
        );
        self.state = State::Closed;
    }

    fn handle_hello(&mut self, payload: &[u8]) -> Result<SessionEvent, SessionError> {
        let msg = Codec::HANDSHAKE.decode(payload)?;
        let Message::Hello(hello) = msg else {
            return Err(SessionError::UnexpectedMessage(
                "first message must be hello".into(),
            ));
        };
        if hello.role != self.config.role.opposite() {
            return Err(SessionError::RoleConflict);
        }
        let version = PROTOCOL_VERSION.min(hello.version);
        if version < MIN_PROTOCOL_VERSION {
            return Err(SessionError::IncompatibleVersion {
                ours: PROTOCOL_VERSION,
                theirs: hello.version,
            });
        }
        let codec = self.negotiate_codec(&hello).ok_or(SessionError::NoCommonCodec)?;
        self.state = State::Established { codec };
        Ok(SessionEvent::Established { version, codec })
    }

    fn negotiate_codec(&self, peer: &Hello) -> Option<Codec> {
        let ours: Vec<&str> = self.config.codecs.iter().map(|c| c.name()).collect();
        let (provider, consumer): (Vec<&str>, Vec<&str>) = match self.config.role {
            PeerRole::Provider => (ours, peer.codecs.iter().map(String::as_str).collect()),
            PeerRole::Consumer => (peer.codecs.iter().map(String::as_str).collect(), ours),
        };
        provider
            .iter()
            .find(|name| consumer.contains(name))
            .and_then(|name| Codec::from_name(name))
    }

    fn queue(&mut self, codec: Codec, msg: &Message) -> Result<(), SessionError> {
        let encoded = codec.encode(msg)?;
        frame_into(&encoded, &mut self.out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::frame;
    use crate::messages::WindowId;

    fn raw_hello_frame(hello: Hello) -> Vec<u8> {
        let encoded = Codec::HANDSHAKE.encode(&Message::Hello(hello)).unwrap();
        frame(&encoded).unwrap()
    }

    fn provider() -> Session {
        Session::new(SessionConfig::new(PeerRole::Provider, "test-provider"))
    }

    fn consumer() -> Session {
        Session::new(SessionConfig::new(PeerRole::Consumer, "test-consumer"))
    }

    #[test]
    fn handshake_establishes_both_sides() {
        let mut p = provider();
        let mut c = consumer();
        let events = c.handle_input(&p.take_output()).unwrap();
        assert!(matches!(
            events[..],
            [SessionEvent::Established { version: 1, codec: Codec::Json }]
        ));
        let events = p.handle_input(&c.take_output()).unwrap();
        assert!(matches!(
            events[..],
            [SessionEvent::Established { version: 1, codec: Codec::Json }]
        ));
        assert!(p.is_established() && c.is_established());
    }

    #[test]
    fn send_before_established_fails() {
        let mut p = provider();
        assert_eq!(
            p.send(&Message::Ping { seq: 1 }),
            Err(SessionError::NotEstablished)
        );
    }

    #[test]
    fn same_role_conflicts() {
        let mut p = provider();
        let other = raw_hello_frame(Hello {
            version: PROTOCOL_VERSION,
            role: PeerRole::Provider,
            codecs: vec!["json".into()],
            name: "imposter".into(),
        });
        assert_eq!(p.handle_input(&other).unwrap_err(), SessionError::RoleConflict);
        assert!(p.is_closed());
        let goodbye = p.take_output();
        assert!(!goodbye.is_empty());
    }

    #[test]
    fn newer_peer_version_negotiates_down_to_ours() {
        let mut p = provider();
        let hello = raw_hello_frame(Hello {
            version: 999,
            role: PeerRole::Consumer,
            codecs: vec!["json".into()],
            name: "future".into(),
        });
        let events = p.handle_input(&hello).unwrap();
        assert!(matches!(
            events[..],
            [SessionEvent::Established { version: PROTOCOL_VERSION, .. }]
        ));
    }

    #[test]
    fn version_zero_is_incompatible() {
        let mut p = provider();
        let hello = raw_hello_frame(Hello {
            version: 0,
            role: PeerRole::Consumer,
            codecs: vec!["json".into()],
            name: "ancient".into(),
        });
        assert_eq!(
            p.handle_input(&hello).unwrap_err(),
            SessionError::IncompatibleVersion {
                ours: PROTOCOL_VERSION,
                theirs: 0
            }
        );
        assert!(p.is_closed());
    }

    #[test]
    fn no_common_codec_fails() {
        let mut p = provider();
        let hello = raw_hello_frame(Hello {
            version: PROTOCOL_VERSION,
            role: PeerRole::Consumer,
            codecs: vec!["carrier-pigeon".into()],
            name: "exotic".into(),
        });
        assert_eq!(p.handle_input(&hello).unwrap_err(), SessionError::NoCommonCodec);
    }

    #[test]
    fn ping_gets_automatic_pong() {
        let mut p = provider();
        let mut c = consumer();
        c.handle_input(&p.take_output()).unwrap();
        p.handle_input(&c.take_output()).unwrap();
        p.send(&Message::Ping { seq: 7 }).unwrap();
        let events = c.handle_input(&p.take_output()).unwrap();
        assert!(events.is_empty());
        let events = p.handle_input(&c.take_output()).unwrap();
        match &events[..] {
            [SessionEvent::Message(Message::Pong { seq })] => assert_eq!(*seq, 7),
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[test]
    fn goodbye_closes_peer() {
        let mut p = provider();
        let mut c = consumer();
        c.handle_input(&p.take_output()).unwrap();
        p.handle_input(&c.take_output()).unwrap();
        p.close("done");
        let events = c.handle_input(&p.take_output()).unwrap();
        match &events[..] {
            [SessionEvent::Closed { reason }] => assert_eq!(reason, "done"),
            other => panic!("unexpected events: {other:?}"),
        }
        assert!(c.is_closed());
        assert!(c.handle_input(b"anything").unwrap().is_empty());
    }

    #[test]
    fn garbage_input_fails_cleanly() {
        let mut p = provider();
        let mut c = consumer();
        c.handle_input(&p.take_output()).unwrap();
        p.handle_input(&c.take_output()).unwrap();
        let garbage = frame(b"not a valid message").unwrap();
        assert!(matches!(
            p.handle_input(&garbage),
            Err(SessionError::Codec(_))
        ));
        assert!(p.is_closed());
    }

    #[test]
    fn hello_after_handshake_is_rejected() {
        let mut p = provider();
        let mut c = consumer();
        c.handle_input(&p.take_output()).unwrap();
        p.handle_input(&c.take_output()).unwrap();
        let extra = raw_hello_frame(Hello {
            version: PROTOCOL_VERSION,
            role: PeerRole::Consumer,
            codecs: vec!["json".into()],
            name: "again".into(),
        });
        assert!(matches!(
            p.handle_input(&extra),
            Err(SessionError::UnexpectedMessage(_))
        ));
    }

    #[test]
    fn send_rejects_managed_messages() {
        let mut p = provider();
        let mut c = consumer();
        c.handle_input(&p.take_output()).unwrap();
        p.handle_input(&c.take_output()).unwrap();
        assert!(matches!(
            p.send(&Message::Goodbye { reason: "x".into() }),
            Err(SessionError::UnexpectedMessage(_))
        ));
        assert!(p.send(&Message::WindowRemoved { window: WindowId(1) }).is_ok());
    }
}
