//! Out-of-band hvsocket transport to the daemon in the WSL distro.
//!
//! The DVC channel stays vestigial; tree data flows over an hvsocket to
//! `accesskit_remoted --vsock`. The WSL VM id comes from the host RDP client's
//! own command line (`/v:<guid>`), which changes on every VM boot — parse it
//! fresh on each connect, never cache it.

use accesskit_remote_client::ClientEvent;
use accesskit_remote_windows::{OutgoingAction, SharedClient};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Guest vsock port the daemon listens on (`accesskit_remoted --vsock`).
pub const DEFAULT_PORT: u32 = 4750;

const READ_TIMEOUT: Duration = Duration::from_millis(50);
const CONNECT_RETRY: Duration = Duration::from_secs(2);

/// Extract the WSL VM id from the host process command line: the value of the
/// `/v:<guid>` argument.
pub fn parse_vm_id(args: impl Iterator<Item = OsString>) -> Option<Uuid> {
    args.filter_map(|a| a.into_string().ok())
        .find_map(|a| a.strip_prefix("/v:").and_then(|v| v.parse().ok()))
}

/// Whether a read error is a timeout to retry rather than a dead connection.
/// hvsocket surfaces receive timeouts as `ConnectionAborted`.
fn is_retryable_read(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionAborted
    )
}

/// Handle to the pump thread; signals shutdown and joins on drop.
pub struct PumpHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl PumpHandle {
    pub fn stop(mut self) {
        self.signal_and_join();
    }

    fn signal_and_join(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for PumpHandle {
    fn drop(&mut self) {
        self.signal_and_join();
    }
}

/// Spawn the hvsocket pump thread: connect (retrying until shutdown), then
/// alternate `take_output`→write and read→`handle_input`, forwarding every
/// [`ClientEvent`] to `on_event` and outgoing UIA actions to the daemon.
pub fn spawn_pump(
    vm_id: Uuid,
    port: u32,
    client: SharedClient,
    actions: Receiver<OutgoingAction>,
    mut on_event: impl FnMut(ClientEvent) + Send + 'static,
) -> PumpHandle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = shutdown.clone();
    let join = std::thread::spawn(move || {
        let Some(stream) = connect_with_retry(vm_id, port, &flag) else {
            return;
        };
        pump(stream, client, actions, &flag, &mut on_event);
    });
    PumpHandle { shutdown, join: Some(join) }
}

fn connect_with_retry(
    vm_id: Uuid,
    port: u32,
    shutdown: &AtomicBool,
) -> Option<accesskit_remote_transport::Socket> {
    let mut logged = false;
    while !shutdown.load(Ordering::Acquire) {
        match accesskit_remote_transport::hvsocket::connect(vm_id, port) {
            Ok(stream) => {
                info!("hvsocket connected to vm {vm_id} port {port}");
                if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
                    warn!("set_read_timeout failed: {e}");
                }
                return Some(stream);
            }
            Err(e) => {
                if !logged {
                    info!("hvsocket connect to vm {vm_id} port {port} failed ({e}); retrying");
                    logged = true;
                }
                std::thread::sleep(CONNECT_RETRY);
            }
        }
    }
    None
}

fn pump(
    mut stream: accesskit_remote_transport::Socket,
    client: SharedClient,
    actions: Receiver<OutgoingAction>,
    shutdown: &AtomicBool,
    on_event: &mut (impl FnMut(ClientEvent) + Send + 'static),
) {
    let mut buf = [0u8; 16384];
    loop {
        if shutdown.load(Ordering::Acquire) {
            let mut locked = client.lock().unwrap();
            locked.close("plugin disconnecting");
            let out = locked.take_output();
            drop(locked);
            if !out.is_empty() {
                let _ = stream.write_all(&out);
            }
            debug!("pump: shutdown");
            return;
        }
        while let Ok((window, request)) = actions.try_recv() {
            if let Err(e) = client.lock().unwrap().request_action(window, request) {
                warn!("pump: action failed: {e:?}");
            }
        }
        let out = client.lock().unwrap().take_output();
        if !out.is_empty() && stream.write_all(&out).is_err() {
            break;
        }
        let events = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => match client.lock().unwrap().handle_input(&buf[..n]) {
                Ok(events) => events,
                Err(e) => {
                    warn!("pump: protocol error: {e:?}");
                    break;
                }
            },
            Err(e) if is_retryable_read(&e) => continue,
            Err(e) => {
                warn!("pump: read error: {e}");
                break;
            }
        };
        for event in events {
            let closed = matches!(event, ClientEvent::Closed { .. });
            on_event(event);
            if closed {
                return;
            }
        }
    }
    info!("pump: connection ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter().map(OsString::from).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_msrdc_command_line() {
        let vm = parse_vm_id(args(&[
            "msrdc.exe",
            "/wslg",
            "/silent",
            "/v:FBDDE2F2-6CC4-4A2A-AC4D-CE69559CADC5",
            "/hvsocketserviceid:00000001-FACB-11E6-BD58-64006A7986D3",
            "/plugin:WSLDVC_PACKAGE",
            r"C:\Program Files\WSL\wslg.rdp",
        ]));
        assert_eq!(vm, Some("FBDDE2F2-6CC4-4A2A-AC4D-CE69559CADC5".parse().unwrap()));
    }

    #[test]
    fn missing_v_arg_is_none() {
        assert_eq!(parse_vm_id(args(&["mstsc.exe", "/wslg", "/silent"])), None);
    }

    #[test]
    fn malformed_guid_is_none() {
        assert_eq!(parse_vm_id(args(&["msrdc.exe", "/v:not-a-guid"])), None);
    }

    #[test]
    fn lowercase_guid_parses() {
        let vm = parse_vm_id(args(&["msrdc.exe", "/v:fbdde2f2-6cc4-4a2a-ac4d-ce69559cadc5"]));
        assert_eq!(vm, Some("fbdde2f2-6cc4-4a2a-ac4d-ce69559cadc5".parse().unwrap()));
    }
}
