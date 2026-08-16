//! `IWTSVirtualChannelCallback`: the inbound half of the DVC data path.
//!
//! Against an RDP server that serves accessibility trees itself — macrdp — this
//! channel *is* the transport, and every byte the provider sends arrives here.
//! In the WSLg arrangement it stays what it always was: a channel that never
//! connects, because the trees come over an hvsocket instead
//! (see [`crate::transport`]).
//!
//! Everything here runs on the RDP client's own thread. Nothing may block on
//! it: the whole session is serviced by that thread, so a slow accessibility
//! path would show up as a frozen desktop.

use accesskit_remote_client::ClientEvent;
use std::sync::Arc;
use tracing::{debug, info, warn};
use windows::Win32::System::RemoteDesktop::{
    IWTSVirtualChannelCallback, IWTSVirtualChannelCallback_Impl,
};
use windows::core::{implement, Result};

use crate::desktop_host::DesktopHost;
use crate::dvc_session::DvcSession;

#[implement(IWTSVirtualChannelCallback)]
pub struct AccessKitChannelCallback {
    name: String,
    session: Arc<DvcSession>,
    host: Arc<DesktopHost>,
}

impl AccessKitChannelCallback {
    pub fn new(name: &str, session: Arc<DvcSession>, host: Arc<DesktopHost>) -> Self {
        Self {
            name: name.to_owned(),
            session,
            host,
        }
    }
}

impl IWTSVirtualChannelCallback_Impl for AccessKitChannelCallback_Impl {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn OnDataReceived(&self, cbsize: u32, pbuffer: *const u8) -> Result<()> {
        if pbuffer.is_null() || cbsize == 0 {
            return Ok(());
        }
        // The pointer is the RDP client's buffer, valid for this call only, so
        // the protocol layer copies whatever it needs before returning.
        let bytes = unsafe { core::slice::from_raw_parts(pbuffer, cbsize as usize) };
        for event in self.session.on_data(bytes) {
            self.route(event);
        }
        Ok(())
    }

    fn OnClose(&self) -> Result<()> {
        info!("[{}] the remote desktop closed the channel", self.name);
        self.session.stop();
        // The hook and heartbeat exist only to serve this channel.
        self.host.stop();
        Ok(())
    }
}

impl AccessKitChannelCallback_Impl {
    /// Turns a protocol event into whatever the host has to be told.
    ///
    /// Window arrivals and departures change the *composition*, which the host
    /// recomputes from the client's state, so they are all one message. Only a
    /// tree delta carries content of its own.
    fn route(&self, event: ClientEvent) {
        match event {
            ClientEvent::Connected => info!("remote desktop session established"),
            ClientEvent::WindowAdded { window } => {
                debug!("remote window added: {}", window.0);
                self.host.sync();
            }
            ClientEvent::WindowRemoved { window } => {
                debug!("remote window removed: {}", window.0);
                self.host.sync();
            }
            ClientEvent::TreeUpdated { window, update } => {
                self.host.delta(window, update);
            }
            ClientEvent::FocusChanged { window } => {
                debug!("remote focus: {:?}", window.map(|w| w.0));
                self.host.sync();
            }
            ClientEvent::Pong { .. } => {}
            ClientEvent::Closed { reason } => {
                warn!("remote desktop session closed: {reason}");
            }
        }
    }
}
