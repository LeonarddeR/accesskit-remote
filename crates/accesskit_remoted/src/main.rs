//! The provider daemon. Serves the demo tree source over a socket until the
//! AT-SPI source lands.
//!
//! Usage: `accesskit_remoted [--tcp PORT | --vsock PORT]`
//! Defaults to `--tcp 4750`; `--vsock` is Linux-only.

mod demo;

use accesskit_remote_server::{ServerConnection, ServerError, ServerEvent};
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

fn main() -> io::Result<()> {
    let (listener, description) = parse_args(std::env::args().skip(1))?;
    eprintln!("accesskit_remoted: listening on {description}");
    loop {
        let stream = listener.accept()?;
        eprintln!("accesskit_remoted: client connected");
        match serve(stream) {
            Ok(()) => eprintln!("accesskit_remoted: client disconnected"),
            Err(e) => eprintln!("accesskit_remoted: connection ended: {e}"),
        }
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> io::Result<(Listener, String)> {
    let args: Vec<String> = args.collect();
    let (mode, port) = match args.as_slice() {
        [] => ("--tcp", DEFAULT_PORT),
        [mode] => (mode.as_str(), DEFAULT_PORT),
        [mode, port] => (
            mode.as_str(),
            port.parse()
                .map_err(|_| io::Error::other(format!("invalid port: {port}")))?,
        ),
        _ => return Err(io::Error::other("usage: accesskit_remoted [--tcp PORT | --vsock PORT]")),
    };
    match mode {
        "--tcp" => {
            let listener = accesskit_remote_transport::tcp::listen_local(port as u16)?;
            Ok((Listener::Tcp(listener), format!("tcp 127.0.0.1:{port}")))
        }
        #[cfg(target_os = "linux")]
        "--vsock" => {
            let listener = accesskit_remote_transport::vsock::listen(port)?;
            Ok((Listener::Vsock(listener), format!("vsock port {port}")))
        }
        other => Err(io::Error::other(format!("unknown mode: {other}"))),
    }
}

fn serve(stream: Socket) -> io::Result<()> {
    let mut source = DemoSource::new();
    let mut server = ServerConnection::new("accesskit_remoted-demo");
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

    let result = pump(&mut server, &mut source, &rx, &mut writer);
    let _ = writer.shutdown(std::net::Shutdown::Both);
    drop(rx);
    let _ = reader_thread.join();
    result
}

fn pump(
    server: &mut ServerConnection,
    source: &mut DemoSource,
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
        let out = server.take_output();
        if !out.is_empty() {
            writer.write_all(&out)?;
        }
        if server.is_closed() {
            return Ok(());
        }
    }
}

fn dispatch(
    server: &mut ServerConnection,
    source: &mut DemoSource,
    chunk: &[u8],
) -> Result<(), ServerError> {
    for event in server.handle_input(chunk)? {
        match event {
            ServerEvent::Established => {
                server.sync_initial_state(source.initial_state(), source.focus())?;
            }
            ServerEvent::Action { window, request } => {
                if let Some(update) = source.perform(window, &request) {
                    server.send_tree_update(window, update)?;
                }
            }
            ServerEvent::Closed { reason } => {
                eprintln!("accesskit_remoted: peer said goodbye: {reason}");
            }
            ServerEvent::Pong { .. } => {}
        }
    }
    Ok(())
}
