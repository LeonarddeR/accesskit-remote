//! `IWTSPlugin` implementation: registers a DVC listener when the RDC client
//! initializes the plug-in.

use tracing::{debug, error, info};
use windows::Win32::Foundation::E_UNEXPECTED;
use windows::Win32::System::RemoteDesktop::{
    IWTSListener, IWTSListenerCallback, IWTSPlugin, IWTSPlugin_Impl, IWTSVirtualChannelManager,
};
use windows::core::{Error, PCSTR, Result, implement};
use windows_core::Ref;

use crate::listener::AccessKitListenerCallback;

/// The channel name the plug-in listens on. Phase-1 placeholder: in `/wslg`
/// there is no server-side AccessKit DVC channel, so the listener never
/// receives a connection — it exists to prove the plug-in is driven by the
/// client. Distinct from the stock `Microsoft::Windows::RDS::RemoteApplicationList`.
pub const CHANNEL_NAME: &str = "AccessKit";

#[implement(IWTSPlugin)]
pub struct AccessKitDvcPlugin;

impl AccessKitDvcPlugin {
    pub fn new() -> Self {
        Self
    }

    fn create_listener(
        &self,
        channel_mgr: &IWTSVirtualChannelManager,
        channel_name: &str,
    ) -> Result<IWTSListener> {
        debug!("Creating listener with name {channel_name}");
        let callback: IWTSListenerCallback =
            AccessKitListenerCallback::new(channel_name.to_owned()).into();
        // Bind the NUL-terminated name to a local so the pointer stays valid
        // for the duration of the CreateListener call.
        let name_c = format!("{channel_name}\0");
        unsafe { channel_mgr.CreateListener(PCSTR::from_raw(name_c.as_ptr()), 0, &callback) }
    }
}

impl IWTSPlugin_Impl for AccessKitDvcPlugin_Impl {
    fn Initialize(&self, pchannelmgr: Ref<'_, IWTSVirtualChannelManager>) -> Result<()> {
        debug!("Initialize");
        let channel_mgr = match pchannelmgr.as_ref() {
            Some(m) => m,
            None => {
                error!("No channel manager given on Initialize");
                return Err(Error::from(E_UNEXPECTED));
            }
        };
        self.create_listener(channel_mgr, CHANNEL_NAME)?;
        Ok(())
    }

    fn Connected(&self) -> Result<()> {
        info!("Client connected");
        Ok(())
    }

    fn Disconnected(&self, dwdisconnectcode: u32) -> Result<()> {
        info!("Client disconnected with {dwdisconnectcode}");
        Ok(())
    }

    fn Terminated(&self) -> Result<()> {
        info!("Client terminated");
        Ok(())
    }
}
