//! One consumer session driven by one tree source, with no I/O of its own.
//!
//! Bytes in, bytes out. [`SourceHost`] holds the [`ServerConnection`] and the
//! [`TreeSource`] together and owns the rule that binds them: an inbound chunk
//! may need the source (a handshake completing wants its initial state, an
//! action wants performing), and the source's own changes need pushing at the
//! connection whenever it has produced any.
//!
//! It exists because that logic is transport-agnostic and there is now more
//! than one transport. The daemon ran it inline over a socket; macrdp runs it
//! inside a `DvcProcessor`, where the same bytes arrive from an RDP dynamic
//! virtual channel instead. Two copies of it would be two places to fix the
//! next ordering bug — and the ordering is not obvious: windows must be
//! announced before their trees, source events polled before the session is
//! established must be discarded rather than queued, and a failure still owes
//! the consumer an explanation.
//!
//! That last one is why failures are [`HostError`] rather than [`ServerError`].
//! `ServerConnection` writes its goodbye into the output buffer *as* it fails,
//! so a caller that returns the error without draining the buffer drops the
//! only reason the consumer will ever be given. Carrying the bytes inside the
//! error makes forgetting them impossible; the daemon had to remember at three
//! separate call sites.

use crate::{apply_source_event, ServerConnection, ServerError, ServerEvent, TreeSource};
use std::time::{Duration, Instant};

/// How often a quiet connection is pinged.
///
/// Both sides answer a `Ping` inside the session layer without the application
/// seeing it, so a live peer always answers promptly and this is pure
/// liveness — it carries no data and means nothing else.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long a peer may go without answering before it is presumed gone.
///
/// Four missed pings. Generous, because the cost of being wrong is dropping a
/// working session, and cheap, because the cost of being slow is only that a
/// dead slot is held a few seconds longer.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(20);

/// A failed step, carrying the bytes that must still reach the consumer.
///
/// Write [`farewell`](Self::farewell) to the transport before closing it.
#[derive(Debug)]
pub struct HostError {
    pub error: ServerError,
    /// The connection's goodbye: the last thing worth sending.
    pub farewell: Vec<u8>,
}

impl core::fmt::Display for HostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for HostError {}

/// A session with one consumer, fed by one tree source.
///
/// The caller supplies bytes from wherever its transport gets them and writes
/// back whatever comes out. Nothing here blocks, allocates a thread, or knows
/// what a socket is.
#[derive(Debug)]
pub struct SourceHost<S> {
    server: ServerConnection,
    source: S,
    peer_goodbye: Option<String>,
    /// When the peer last proved it was there, and the next ping's number.
    /// `None` until the session is established, because there is nobody to
    /// ping before that.
    heard_from: Option<Instant>,
    pinged: Option<Instant>,
    seq: u64,
    /// Whether the session ended because [`heartbeat`](Self::heartbeat) gave up.
    gave_up: bool,
}

impl<S: TreeSource> SourceHost<S> {
    /// Wraps a source in a session announced under `name`.
    pub fn new(name: impl Into<String>, source: S) -> Self {
        Self {
            server: ServerConnection::new(name),
            source,
            peer_goodbye: None,
            heard_from: None,
            pinged: None,
            seq: 0,
            gave_up: false,
        }
    }

    /// Feeds a chunk from the consumer and returns what to send back.
    ///
    /// Chunk boundaries are irrelevant — the wire's framing reassembles
    /// messages from arbitrary splits, which is what makes a DVC, a socket and
    /// a test all equivalent here.
    pub fn handle_input(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HostError> {
        // Bytes arriving at all are proof of life — a pong is only the kind we
        // ask for when there is nothing else.
        self.heard_from = Some(Instant::now());
        let events = self.server.handle_input(chunk).map_err(|error| self.fail(error))?;
        for event in events {
            match event {
                ServerEvent::Established => {
                    let (windows, focus) = self.source.initial_state();
                    self.server
                        .sync_initial_state(windows, focus)
                        .map_err(|error| self.fail(error))?;
                }
                ServerEvent::Action { window, request } => {
                    self.source.perform(window, &request);
                }
                ServerEvent::Closed { reason } => self.peer_goodbye = Some(reason),
                ServerEvent::Pong { .. } => {}
            }
        }
        Ok(self.server.take_output())
    }

    /// Drains whatever the source has observed and returns what to send.
    ///
    /// Call this regularly — it is the only path by which a tree change
    /// reaches the consumer, and nothing else will notice that the source has
    /// something to say. Returns an empty vector when there is nothing to send,
    /// which is the common case.
    ///
    /// **Events polled before the session is established are discarded, not
    /// queued.** They describe a desktop the consumer has not been told about
    /// yet; the full picture arrives with `initial_state` at establishment, and
    /// a delta applied before it would name windows that do not exist.
    ///
    /// It still returns bytes in that state, and must: the session queues its
    /// own `Hello` the moment it is constructed, and that handshake opener is
    /// what the first `pump` — before any input has arrived — exists to carry.
    /// A transport that only pumps once established would wait forever for a
    /// consumer that is itself waiting to be greeted.
    pub fn pump(&mut self) -> Result<Vec<u8>, HostError> {
        let events = self.source.poll_events();
        if self.server.is_established() {
            for event in events {
                apply_source_event(&mut self.server, event).map_err(|error| self.fail(error))?;
            }
        }
        Ok(self.server.take_output())
    }

    /// Pings a quiet peer, and gives up on one that has stopped answering.
    ///
    /// **Without this a connection can never end.** A transport that dies
    /// without saying so — an SSH tunnel dropped, a client killed, a laptop
    /// closed — leaves a socket that accepts writes and delivers nothing, and a
    /// provider with nothing to say writes nothing, so nothing ever fails. The
    /// connection is then held open forever; against a server that serves one
    /// client at a time, that is a daemon that refuses every later client while
    /// looking perfectly healthy. Observed exactly that way: the port still
    /// accepted TCP, and every subsequent client timed out.
    ///
    /// On timeout the session is closed, so the caller's `is_closed` becomes
    /// true and its loop ends the way it would for any other goodbye. Takes the
    /// time rather than reading a clock, so the behaviour is testable.
    pub fn heartbeat(&mut self, now: Instant) -> Result<Vec<u8>, HostError> {
        if !self.server.is_established() {
            return Ok(Vec::new());
        }
        let heard_from = *self.heard_from.get_or_insert(now);
        if now.duration_since(heard_from) >= PEER_TIMEOUT {
            self.server.close("peer stopped answering");
            self.gave_up = true;
            return Ok(self.server.take_output());
        }
        let due = self
            .pinged
            .is_none_or(|pinged| now.duration_since(pinged) >= PING_INTERVAL);
        if !due {
            return Ok(Vec::new());
        }
        self.pinged = Some(now);
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        self.server.send_ping(seq).map_err(|error| self.fail(error))?;
        Ok(self.server.take_output())
    }

    /// Closes the session, returning the goodbye to send.
    pub fn close(&mut self, reason: impl Into<String>) -> Vec<u8> {
        self.server.close(reason);
        self.server.take_output()
    }

    pub fn is_established(&self) -> bool {
        self.server.is_established()
    }

    pub fn is_closed(&self) -> bool {
        self.server.is_closed()
    }

    /// The reason the consumer gave for leaving, if it said goodbye rather than
    /// simply vanishing.
    pub fn peer_goodbye(&self) -> Option<&str> {
        self.peer_goodbye.as_deref()
    }

    /// Whether this session ended because the peer stopped answering.
    ///
    /// A caller's loop sees only [`is_closed`](Self::is_closed), and every way
    /// a session can end looks identical there. It is not identical to
    /// diagnose: a peer that said goodbye is a normal shutdown, while a peer
    /// that went silent is a hung or crashed consumer, and the difference is
    /// the first thing worth knowing. Cost of not distinguishing them, measured
    /// once: a tree source that vanished mid-session with nothing in any log to
    /// say why.
    pub fn gave_up_on_peer(&self) -> bool {
        self.gave_up
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    fn fail(&mut self, error: ServerError) -> HostError {
        HostError {
            error,
            farewell: self.server.take_output(),
        }
    }
}

/// So a `Box<dyn TreeSource>` can be hosted, as the daemon's runtime choice of
/// source requires.
impl<T: TreeSource + ?Sized> TreeSource for Box<T> {
    fn initial_state(
        &mut self,
    ) -> (Vec<(crate::WindowDescriptor, accesskit::TreeUpdate)>, Option<accesskit_remote::WindowId>)
    {
        (**self).initial_state()
    }

    fn perform(&mut self, window: accesskit_remote::WindowId, request: &accesskit::ActionRequest) {
        (**self).perform(window, request);
    }

    fn poll_events(&mut self) -> Vec<crate::SourceEvent> {
        (**self).poll_events()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceEvent, WindowDescriptor};
    use accesskit_remote::{AppInfo, Message, PeerRole, Session, SessionConfig, WindowId};

    /// A source that reports a fixed initial state and hands out queued events.
    #[derive(Default)]
    struct StubSource {
        events: Vec<SourceEvent>,
        windows: Vec<(WindowDescriptor, accesskit::TreeUpdate)>,
        performed: Vec<WindowId>,
    }

    impl TreeSource for StubSource {
        fn initial_state(
            &mut self,
        ) -> (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>) {
            (core::mem::take(&mut self.windows), None)
        }
        fn perform(&mut self, window: WindowId, _request: &accesskit::ActionRequest) {
            self.performed.push(window);
        }
        fn poll_events(&mut self) -> Vec<SourceEvent> {
            core::mem::take(&mut self.events)
        }
    }

    fn window(id: u64) -> (WindowDescriptor, accesskit::TreeUpdate) {
        let root = accesskit::NodeId(1);
        let update = accesskit::TreeUpdate {
            nodes: vec![(root, accesskit::Node::new(accesskit::Role::Window))],
            tree: Some(accesskit::Tree::new(root)),
            tree_id: accesskit::TreeId::ROOT,
            focus: root,
        };
        let descriptor = WindowDescriptor {
            id: WindowId(id),
            title: format!("window {id}"),
            app: AppInfo {
                name: "test".into(),
                app_id: None,
                pid: None,
                toolkit: None,
                toolkit_version: None,
            },
            native_window_id: None,
        };
        (descriptor, update)
    }

    /// Drives a consumer far enough to establish, returning its side.
    fn consumer() -> Session {
        Session::new(SessionConfig::new(PeerRole::Consumer, "test-consumer"))
    }

    /// **The first pump carries the handshake, not a tree.** A host that only
    /// produced bytes once established would never greet anyone.
    #[test]
    fn the_first_pump_carries_the_hello_that_starts_the_handshake() {
        let mut host = SourceHost::new("test", StubSource::default());
        assert!(!host.pump().unwrap().is_empty(), "nothing has arrived yet, and there is still something to say");
    }

    #[test]
    fn a_source_event_before_establishment_is_dropped_rather_than_queued() {
        let mut host = SourceHost::new("test", StubSource::default());
        let _hello = host.pump().unwrap();
        // Nothing has connected, so there is nobody the removal could mean
        // anything to — and applying it would name an unannounced window.
        *host.source_mut() = StubSource {
            events: vec![SourceEvent::WindowRemoved(WindowId(7))],
            ..Default::default()
        };
        assert!(host.pump().unwrap().is_empty());
        assert!(!host.is_closed(), "and it is not an error either");
        assert!(
            host.source_mut().poll_events().is_empty(),
            "dropped means consumed: a queue that survives would replay a stale desktop later",
        );
    }

    #[test]
    fn a_source_event_after_establishment_reaches_the_consumer() {
        let mut host = SourceHost::new("test", StubSource::default());
        let mut consumer = consumer();
        let reply = host.handle_input(&consumer.take_output()).unwrap();
        consumer.handle_input(&reply).unwrap();
        assert!(host.is_established());

        let (descriptor, tree) = window(1);
        *host.source_mut() =
            StubSource { events: vec![SourceEvent::WindowAdded { descriptor, tree }], ..Default::default() };
        let out = host.pump().unwrap();

        let events = consumer.handle_input(&out).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            accesskit_remote::SessionEvent::Message(Message::WindowAdded { .. })
        )));
    }

    #[test]
    fn establishing_sends_the_source_s_initial_state() {
        let mut host =
            SourceHost::new("test", StubSource { windows: vec![window(1)], ..Default::default() });
        let mut consumer = consumer();

        // One exchange establishes both sides: each queues its own Hello on
        // construction, so the host's reply carries its greeting and, behind
        // it, everything the source already knew about.
        let reply = host.handle_input(&consumer.take_output()).unwrap();
        assert!(host.is_established());
        let events = consumer.handle_input(&reply).unwrap();

        let announced = events.iter().any(|event| {
            matches!(
                event,
                accesskit_remote::SessionEvent::Message(Message::WindowAdded { window, .. })
                    if *window == WindowId(1)
            )
        });
        assert!(
            announced,
            "the window the source already had is announced without being asked for again",
        );
    }

    /// Establishes both sides and hands back the consumer.
    fn established() -> (SourceHost<StubSource>, Session) {
        let mut host = SourceHost::new("test", StubSource::default());
        let mut consumer = consumer();
        let reply = host.handle_input(&consumer.take_output()).unwrap();
        consumer.handle_input(&reply).unwrap();
        assert!(host.is_established());
        (host, consumer)
    }

    #[test]
    fn nothing_is_pinged_before_the_handshake() {
        let mut host = SourceHost::new("test", StubSource::default());
        let _hello = host.pump().unwrap();
        assert!(host.heartbeat(Instant::now()).unwrap().is_empty());
        assert!(!host.is_closed(), "and an unestablished session never times out");
    }

    /// **The wedge.** A transport that dies without saying so leaves a socket
    /// that accepts writes and delivers nothing. A provider with nothing to say
    /// writes nothing, so nothing ever fails, and the connection is held
    /// forever — which, against a one-client-at-a-time server, is a daemon that
    /// refuses every later client while looking healthy.
    #[test]
    fn a_peer_that_stops_answering_is_given_up_on() {
        let (mut host, _consumer) = established();
        let start = Instant::now();

        // It is pinged first, and the ping is not mistaken for an answer.
        let ping = host.heartbeat(start + PING_INTERVAL).unwrap();
        assert!(!ping.is_empty(), "a quiet peer is asked whether it is there");
        assert!(!host.is_closed());

        let farewell = host.heartbeat(start + PEER_TIMEOUT).unwrap();
        assert!(host.is_closed(), "silence for the whole timeout ends the session");
        assert!(!farewell.is_empty(), "and the peer is told why, in case it is listening");
        assert!(
            host.gave_up_on_peer(),
            "and the host says so, or its caller has no way to tell this from a normal goodbye",
        );
        assert!(host.peer_goodbye().is_none(), "the peer said nothing — that is the point");
    }

    /// A peer that answers keeps its session, however little it has to say —
    /// an idle desktop is the normal case, not a symptom.
    #[test]
    fn a_quiet_but_answering_peer_keeps_its_session() {
        let (mut host, mut consumer) = established();
        let start = Instant::now();

        let ping = host.heartbeat(start + PING_INTERVAL).unwrap();
        assert!(!ping.is_empty());
        // The session layer answers a ping without the application seeing it,
        // so this is what any conforming consumer does.
        consumer.handle_input(&ping).unwrap();
        let pong = consumer.take_output();
        assert!(!pong.is_empty(), "the consumer answers by itself");
        host.handle_input(&pong).unwrap();

        let _ = host.heartbeat(start + PEER_TIMEOUT).unwrap();
        assert!(!host.is_closed(), "it answered, so the clock started again");
        assert!(!host.gave_up_on_peer());
    }

    #[test]
    fn a_failure_carries_the_goodbye_the_consumer_is_owed() {
        let mut host = SourceHost::new("test", StubSource::default());
        // Garbage that cannot be a handshake: the session closes itself and
        // writes its reason, which the error must not swallow.
        let err = host.handle_input(&[0xff; 64]).unwrap_err();
        assert!(!err.farewell.is_empty(), "the reason must still reach the consumer");
    }
}
