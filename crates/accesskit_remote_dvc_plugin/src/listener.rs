//! `IWTSListenerCallback`: accepts an incoming channel connection and hands
//! back a channel callback.
//!
//! The connection is where the full-desktop session actually begins. Nothing is
//! built until a provider opens the channel, which is the point at which we know
//! there is a remote desktop to read at all — an RDP server without the
//! accessibility channel never gets this far, and the plug-in stays inert.

use accesskit_remote_client::ClientConnection;
use accesskit_remote_windows::SharedClient;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{E_POINTER, E_UNEXPECTED};
use windows::Win32::System::RemoteDesktop::{
    IWTSListenerCallback, IWTSListenerCallback_Impl, IWTSVirtualChannel, IWTSVirtualChannelCallback,
};
use windows::core::{implement, BSTR, Error, Result};
use windows_core::{BOOL, OutRef, Ref};

use crate::channel::AccessKitChannelCallback;
use crate::desktop_host::{self, DesktopHost};
use crate::dvc_session::DvcSession;

#[implement(IWTSListenerCallback)]
pub struct AccessKitListenerCallback {
    name: String,
    /// Kept so the hook and heartbeat threads outlive the connection that
    /// started them, and are stopped once, on plug-in teardown.
    host: Mutex<Option<Arc<DesktopHost>>>,
}

impl AccessKitListenerCallback {
    pub fn new(name: String) -> Self {
        Self {
            name,
            host: Mutex::new(None),
        }
    }
}

impl IWTSListenerCallback_Impl for AccessKitListenerCallback_Impl {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn OnNewChannelConnection(
        &self,
        pchannel: Ref<'_, IWTSVirtualChannel>,
        _data: &BSTR,
        pbaccept: *mut BOOL,
        ppcallback: OutRef<'_, IWTSVirtualChannelCallback>,
    ) -> Result<()> {
        debug!("new channel connection on listener {}", self.name);
        let Some(channel) = pchannel.as_ref() else {
            return Err(Error::from(E_UNEXPECTED));
        };
        if pbaccept.is_null() {
            return Err(Error::from(E_POINTER));
        }

        let client: SharedClient =
            Arc::new(Mutex::new(ClientConnection::new("accesskit_dvc_plugin")));
        let (actions_tx, actions_rx) = std::sync::mpsc::channel();

        // Find and claim the session window before greeting the provider: the
        // trees start arriving as soon as the handshake completes, and they
        // need somewhere to go.
        let host = Arc::new(desktop_host::start(
            client.clone(),
            actions_tx,
            "Remote desktop",
        ));
        *self.host.lock().unwrap() = Some(host.clone());

        let session = match DvcSession::start(channel, client, actions_rx) {
            Ok(session) => session,
            Err(e) => {
                warn!("could not take the accessibility channel: {e}");
                return Err(e);
            }
        };
        info!("reading the remote desktop's accessibility tree");

        unsafe { *pbaccept = BOOL::from(true) };
        let callback: IWTSVirtualChannelCallback =
            AccessKitChannelCallback::new(&self.name, session, host).into();
        ppcallback.write(callback.into())?;
        Ok(())
    }
}
