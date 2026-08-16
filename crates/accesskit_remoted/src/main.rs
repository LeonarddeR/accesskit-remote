//! The provider daemon. Serves a tree source over a socket.
//!
//! Usage: `accesskit_remoted [--tcp PORT | --vsock PORT] [--atspi | --ax]`
//! Defaults to `--tcp 4750` with the demo source; `--vsock` and `--atspi`
//! are Linux-only, `--ax` is macOS-only.

mod demo;
// Only the AT-SPI path wraps the source, so the module is unused off Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod wslg;

use accesskit_remote_server::{HostError, SourceHost, TreeSource};
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
    #[cfg(target_os = "macos")]
    Ax,
}

#[cfg(target_os = "linux")]
fn select_atspi() -> io::Result<Source> {
    Ok(Source::Atspi)
}

#[cfg(not(target_os = "linux"))]
fn select_atspi() -> io::Result<Source> {
    Err(io::Error::other("--atspi is only supported on Linux"))
}

#[cfg(target_os = "macos")]
fn select_ax() -> io::Result<Source> {
    Ok(Source::Ax)
}

#[cfg(not(target_os = "macos"))]
fn select_ax() -> io::Result<Source> {
    Err(io::Error::other("--ax is only supported on macOS"))
}

fn main() -> io::Result<()> {
    // Level via ACCESSKIT_REMOTED_LOG (default info), mirroring the DVC
    // plug-in's ACCESSKIT_DVC_LOG.
    let level = std::env::var("ACCESSKIT_REMOTED_LOG")
        .ok()
        .and_then(|value| value.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .compact()
        .with_ansi(false)
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
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
            "--ax" => source = select_ax()?,
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
                "usage: accesskit_remoted [--tcp PORT | --vsock PORT] [--atspi | --ax]",
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
    let (source, name): (Box<dyn TreeSource>, &str) = match source {
        Source::Demo => (Box::new(DemoSource::new()), "accesskit_remoted-demo"),
        #[cfg(target_os = "linux")]
        Source::Atspi => {
            let inner = accesskit_remote_atspi::AtspiSource::new().map_err(io::Error::other)?;
            let source: Box<dyn TreeSource> = match wslg::WestonLogTail::open_default() {
                Some(tail) => {
                    eprintln!("accesskit_remoted: enriching window ids from the weston log");
                    Box::new(wslg::WslgSource::new(inner, tail))
                }
                None => Box::new(inner),
            };
            (source, "accesskit_remoted-atspi")
        }
        #[cfg(target_os = "macos")]
        Source::Ax => {
            // The Accessibility grant is checked here rather than at start-up:
            // a daemon that outlives a revoked grant should fail the next
            // connection with the explanation, not have exited hours earlier.
            let source = accesskit_remote_ax::AxSource::new().map_err(io::Error::other)?;
            (Box::new(source) as Box<dyn TreeSource>, "accesskit_remoted-ax")
        }
    };
    let mut host = SourceHost::new(name, source);
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

    let result = pump(&mut host, &rx, &mut writer);
    let _ = writer.shutdown(std::net::Shutdown::Both);
    drop(rx);
    let _ = reader_thread.join();
    result
}

/// Moves bytes between the socket and the host until either end is done.
///
/// The 50ms timeout is what makes source-driven traffic possible at all: a
/// blocking read would only ever wake on consumer input, and the consumer has
/// nothing to say while the desktop changes underneath it.
fn pump(
    host: &mut SourceHost<Box<dyn TreeSource>>,
    rx: &mpsc::Receiver<Vec<u8>>,
    writer: &mut Socket,
) -> io::Result<()> {
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => write_or_explain(host.handle_input(&chunk), writer)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        write_or_explain(host.pump(), writer)?;
        if let Some(reason) = host.peer_goodbye() {
            eprintln!("accesskit_remoted: peer said goodbye: {reason}");
        }
        if host.is_closed() {
            return Ok(());
        }
    }
}

/// Writes whatever the host produced — including the goodbye it produced while
/// failing, which is the consumer's only explanation.
fn write_or_explain(
    produced: Result<Vec<u8>, HostError>,
    writer: &mut Socket,
) -> io::Result<()> {
    match produced {
        Ok(out) => {
            if !out.is_empty() {
                writer.write_all(&out)?;
            }
            Ok(())
        }
        Err(e) => {
            let _ = writer.write_all(&e.farewell);
            Err(io::Error::other(e))
        }
    }
}
