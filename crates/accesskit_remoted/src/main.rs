//! The provider daemon. Serves a tree source over a socket.
//!
//! Usage: `accesskit_remoted [--tcp PORT | --vsock PORT] [--atspi]`
//! Defaults to `--tcp 4750` with the demo source; `--vsock` and `--atspi`
//! are Linux-only.

mod demo;

use accesskit_remote_server::{
    apply_source_event, ServerConnection, ServerError, ServerEvent, TreeSource,
};
use accesskit_remote_transport::Socket;
use demo::DemoSource;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::Duration;

pub const DEFAULT_PORT: u32 = 4750;

enum Listener {
    Tcp(std::net::TcpListener),
    #[cfg(target_os = "linux")]
    Vsock(Socket),
}

impl Listener {
    fn accept(&self) -> io::Result<Socket> {
        match self {
            Self::Tcp(listener) => listener.accept().map(|(stream, _)| stream.into()),
            #[cfg(target_os = "linux")]
            Self::Vsock(listener) => listener.accept().map(|(socket, _)| socket),
        }
    }
}

/// Which tree source the daemon serves to each connection.
#[derive(Clone, Copy)]
enum Source {
    Demo,
    #[cfg(target_os = "linux")]
    Atspi,
}

#[cfg(target_os = "linux")]
fn select_atspi() -> io::Result<Source> {
    Ok(Source::Atspi)
}

#[cfg(not(target_os = "linux"))]
fn select_atspi() -> io::Result<Source> {
    Err(io::Error::other("--atspi is only supported on Linux"))
}

fn main() -> io::Result<()> {
    let (listener, description, source) = parse_args(std::env::args().skip(1))?;
    eprintln!("accesskit_remoted: listening on {description}");
    loop {
        let stream = listener.accept()?;
        eprintln!("accesskit_remoted: client connected");
        match serve(stream, source) {
            Ok(()) => eprintln!("accesskit_remoted: client disconnected"),
            Err(e) => eprintln!("accesskit_remoted: connection ended: {e}"),
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> io::Result<(Listener, String, Source)> {
    let mut source = Source::Demo;
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--atspi" => source = select_atspi()?,
            _ => positional.push(arg),
        }
    }
    let (mode, port) = match positional.as_slice() {
        [] => ("--tcp", DEFAULT_PORT),
        [mode] => (mode.as_str(), DEFAULT_PORT),
        [mode, port] => (
            mode.as_str(),
            port.parse()
                .map_err(|_| io::Error::other(format!("invalid port: {port}")))?,
        ),
        _ => {
            return Err(io::Error::other(
                "usage: accesskit_remoted [--tcp PORT | --vsock PORT] [--atspi]",
            ))
        }
    };
    match mode {
        "--tcp" => {
            let listener = accesskit_remote_transport::tcp::listen_local(port as u16)?;
            Ok((Listener::Tcp(listener), format!("tcp 127.0.0.1:{port}"), source))
        }
        #[cfg(target_os = "linux")]
        "--vsock" => {
            let listener = accesskit_remote_transport::vsock::listen(port)?;
            Ok((Listener::Vsock(listener), format!("vsock port {port}"), source))
        }
        other => Err(io::Error::other(format!("unknown mode: {other}"))),
    }
}

fn serve(stream: Socket, source: Source) -> io::Result<()> {
    let (mut source, name): (Box<dyn TreeSource>, &str) = match source {
        Source::Demo => (Box::new(DemoSource::new()), "accesskit_remoted-demo"),
        #[cfg(target_os = "linux")]
        Source::Atspi => (
            Box::new(accesskit_remote_atspi::AtspiSource::new().map_err(io::Error::other)?),
            "accesskit_remoted-atspi",
        ),
    };
    let mut server = ServerConnection::new(name);
    let mut writer = stream.try_clone()?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    let reader_thread = std::thread::spawn(move || {
        let mut reader = stream;
        let mut buf = [0u8; 16384];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let result = pump(&mut server, source.as_mut(), &rx, &mut writer);
    let _ = writer.shutdown(std::net::Shutdown::Both);
    drop(rx);
    let _ = reader_thread.join();
    result
}

fn pump(
    server: &mut ServerConnection,
    source: &mut dyn TreeSource,
    rx: &mpsc::Receiver<Vec<u8>>,
    writer: &mut Socket,
) -> io::Result<()> {
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                if let Err(e) = dispatch(server, source, &chunk) {
                    let _ = writer.write_all(&server.take_output());
                    return Err(io::Error::other(e));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if let Err(e) = drain_source(server, source) {
            let _ = writer.write_all(&server.take_output());
            return Err(io::Error::other(e));
        }
        let out = server.take_output();
        if !out.is_empty() {
            writer.write_all(&out)?;
        }
        if server.is_closed() {
            return Ok(());
        }
    }
}

/// Applies the source's buffered changes to the connection, but only once
/// the session is established; events polled before that are dropped.
fn drain_source(server: &mut ServerConnection, source: &mut dyn TreeSource) -> Result<(), ServerError> {
    let events = source.poll_events();
    if !server.is_established() {
        return Ok(());
    }
    for event in events {
        apply_source_event(server, event)?;
    }
    Ok(())
}

fn dispatch(
    server: &mut ServerConnection,
    source: &mut dyn TreeSource,
    chunk: &[u8],
) -> Result<(), ServerError> {
    for event in server.handle_input(chunk)? {
        match event {
            ServerEvent::Established => {
                let (windows, focus) = source.initial_state();
                server.sync_initial_state(windows, focus)?;
            }
            ServerEvent::Action { window, request } => {
                source.perform(window, &request);
            }
            ServerEvent::Closed { reason } => {
                eprintln!("accesskit_remoted: peer said goodbye: {reason}");
            }
            ServerEvent::Pong { .. } => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit_remote::{
        AppInfo, Message, PeerRole, Session, SessionConfig, SessionEvent, WindowId,
    };
    use accesskit_remote_server::{SourceEvent, WindowDescriptor};

    struct StubSource {
        events: Vec<SourceEvent>,
    }

    impl TreeSource for StubSource {
        fn initial_state(
            &mut self,
        ) -> (Vec<(WindowDescriptor, accesskit::TreeUpdate)>, Option<WindowId>) {
            (Vec::new(), None)
        }
        fn perform(&mut self, _window: WindowId, _request: &accesskit::ActionRequest) {}
        fn poll_events(&mut self) -> Vec<SourceEvent> {
            std::mem::take(&mut self.events)
        }
    }

    fn established_server() -> (ServerConnection, Session) {
        let mut server = ServerConnection::new("test");
        let mut consumer = Session::new(SessionConfig::new(PeerRole::Consumer, "consumer"));
        consumer.handle_input(&server.take_output()).unwrap();
        server.handle_input(&consumer.take_output()).unwrap();
        assert!(server.is_established());
        (server, consumer)
    }

    fn empty_tree() -> accesskit::TreeUpdate {
        accesskit::TreeUpdate {
            nodes: vec![(
                accesskit::NodeId(0),
                accesskit::Node::new(accesskit::Role::Window),
            )],
            tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
            tree_id: accesskit::TreeId::ROOT,
            focus: accesskit::NodeId(0),
        }
    }

    #[test]
    fn drain_forwards_events_when_established() {
        let (mut server, mut consumer) = established_server();
        let mut source = StubSource {
            events: vec![SourceEvent::WindowAdded {
                descriptor: WindowDescriptor {
                    id: WindowId(1),
                    title: "w".into(),
                    app: AppInfo::default(),
                    native_window_id: None,
                },
                tree: empty_tree(),
            }],
        };
        drain_source(&mut server, &mut source).unwrap();
        let events = consumer.handle_input(&server.take_output()).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, SessionEvent::Message(Message::WindowAdded { .. }))));
    }

    #[test]
    fn drain_drops_events_before_established() {
        let mut server = ServerConnection::new("test");
        let _ = server.take_output(); // discard the queued handshake hello
        let mut source = StubSource {
            events: vec![SourceEvent::FocusChanged(None)],
        };
        drain_source(&mut server, &mut source).unwrap();
        assert!(
            server.take_output().is_empty(),
            "no session messages emitted before established"
        );
        assert!(
            source.poll_events().is_empty(),
            "buffered events were drained and dropped"
        );
    }
}
