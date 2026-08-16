//! The dynamic virtual channel as the data path.
//!
//! In the WSLg arrangement the DVC is vestigial: the trees come from a daemon
//! inside the WSL VM over an hvsocket, and this channel never connects (see
//! [`crate::transport`]). Against an RDP *server* that serves the trees itself —
//! macrdp — the channel is the transport, and this module is it.
//!
//! Three things have to happen on three different threads, which is most of the
//! design:
//!
//! - **Inbound** bytes arrive on the RDP client's own thread, in
//!   `IWTSVirtualChannelCallback::OnDataReceived`. Nothing may block there; the
//!   client's whole session runs on it.
//! - **Outbound** protocol bytes are produced by the same call, and are written
//!   straight back.
//! - **Actions** come from UI Automation, on whatever thread the screen reader
//!   poked, and must be written when no inbound data is in flight. That needs a
//!   thread of its own.
//!
//! Which is why the channel is held as an [`AgileReference`]: writing to a COM
//! interface from a thread other than the one it arrived on is only legal
//! through a marshalled proxy, and an agile reference is that proxy — a no-op
//! when the object is already agile, correct marshalling when it is not. Doing
//! it directly appears to work until it is tried under an apartment that cares,
//! and then fails as `RPC_E_WRONG_THREAD` in the field.

use accesskit_remote_client::{ClientConnection, ClientEvent};
use accesskit_remote_windows::{OutgoingAction, SharedClient};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, info, warn};
use windows::Win32::System::RemoteDesktop::IWTSVirtualChannel;
use windows_core::AgileReference;

/// How often the action pump looks for something to send.
///
/// It is the latency a user feels between activating a control in their screen
/// reader and the remote application being told, so it is short; the loop does
/// nothing but a `try_recv` when idle.
const ACTION_POLL: Duration = Duration::from_millis(20);

/// One open `AccessKit` channel.
pub struct DvcSession {
    client: SharedClient,
    channel: AgileReference<IWTSVirtualChannel>,
    shutdown: Arc<AtomicBool>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl DvcSession {
    /// Takes the channel, greets the provider, and starts the action pump.
    ///
    /// Events the provider sends come back from [`on_data`](Self::on_data),
    /// on the RDP client's thread; this only owns the outbound direction.
    pub fn start(
        channel: &IWTSVirtualChannel,
        client: SharedClient,
        actions: Receiver<OutgoingAction>,
    ) -> windows_core::Result<Arc<Self>> {
        let session = Arc::new(Self {
            client,
            channel: AgileReference::new(channel)?,
            shutdown: Arc::new(AtomicBool::new(false)),
            pump: Mutex::new(None),
        });

        // The consumer speaks first: its Hello is queued the moment the
        // connection is constructed, and the provider is waiting for it.
        session.flush();

        let pump_session = session.clone();
        let join = std::thread::spawn(move || {
            while !pump_session.shutdown.load(Ordering::Acquire) {
                let mut sent = false;
                while let Ok((window, request)) = actions.try_recv() {
                    info!(
                        action = ?request.action,
                        node = request.target_node.0,
                        window = window.0,
                        "forwarding an action to the remote desktop"
                    );
                    match pump_session
                        .client
                        .lock()
                        .unwrap()
                        .request_action(window, request)
                    {
                        Ok(()) => sent = true,
                        Err(e) => warn!("action rejected: {e}"),
                    }
                }
                if sent {
                    pump_session.flush();
                }
                std::thread::sleep(ACTION_POLL);
            }
            debug!("action pump stopped");
        });
        *session.pump.lock().unwrap() = Some(join);
        Ok(session)
    }

    /// Feeds bytes from the provider and returns the events they produced.
    ///
    /// Runs on the RDP client's thread.
    pub fn on_data(&self, bytes: &[u8]) -> Vec<ClientEvent> {
        let events = {
            let mut client = self.client.lock().unwrap();
            match client.handle_input(bytes) {
                Ok(events) => events,
                Err(e) => {
                    warn!("protocol error from the remote desktop: {e}");
                    Vec::new()
                }
            }
        };
        self.flush();
        events
    }

    /// Writes whatever the connection has queued. Safe from any thread.
    pub fn flush(&self) {
        let out = self.client.lock().unwrap().take_output();
        if out.is_empty() {
            return;
        }
        let channel = match self.channel.resolve() {
            Ok(channel) => channel,
            Err(e) => {
                warn!("cannot reach the channel to write {} bytes: {e}", out.len());
                return;
            }
        };
        if let Err(e) = unsafe { channel.Write(&out, None) } {
            warn!("writing {} bytes to the channel failed: {e}", out.len());
        }
    }

    /// The channel closed: stop the pump and let the provider go.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
        let join = self.pump.lock().unwrap().take();
        if let Some(join) = join {
            let _ = join.join();
        }
    }
}

/// A fresh connection, ready to greet a provider.
pub fn new_client() -> ClientConnection {
    ClientConnection::new("accesskit_dvc_plugin")
}
