//! `IWTSVirtualChannelCallback` implementation.
//!
//! Phase-1 stub: logs and drops received data. The real data path is an
//! out-of-band hvsocket to `accesskit_remoted` (a later milestone), not this
//! DVC channel, which never connects in `/wslg`.

use tracing::debug;
use windows::Win32::System::RemoteDesktop::{
    IWTSVirtualChannelCallback, IWTSVirtualChannelCallback_Impl,
};
use windows::core::{Result, implement};

#[implement(IWTSVirtualChannelCallback)]
pub struct AccessKitChannelCallback {
    name: String,
}

impl AccessKitChannelCallback {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_owned() }
    }
}

impl IWTSVirtualChannelCallback_Impl for AccessKitChannelCallback_Impl {
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn OnDataReceived(&self, cbsize: u32, _pbuffer: *const u8) -> Result<()> {
        debug!("[{}] OnDataReceived: {cbsize} bytes (stub, dropped)", self.name);
        Ok(())
    }

    fn OnClose(&self) -> Result<()> {
        debug!("[{}] OnClose (stub)", self.name);
        Ok(())
    }
}
