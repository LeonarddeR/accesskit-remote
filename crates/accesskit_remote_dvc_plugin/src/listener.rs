//! `IWTSListenerCallback` implementation: accepts an incoming channel
//! connection and hands back a channel callback.

use tracing::debug;
use windows::Win32::Foundation::{E_POINTER, E_UNEXPECTED};
use windows::Win32::System::RemoteDesktop::{
    IWTSListenerCallback, IWTSListenerCallback_Impl, IWTSVirtualChannel, IWTSVirtualChannelCallback,
};
use windows::core::{BSTR, Error, Result, implement};
use windows_core::{BOOL, OutRef, Ref};

use crate::channel::AccessKitChannelCallback;

#[implement(IWTSListenerCallback)]
pub struct AccessKitListenerCallback {
    name: String,
}

impl AccessKitListenerCallback {
    pub fn new(name: String) -> Self {
        Self { name }
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
        debug!("New channel connection on listener {}", self.name);
        if pchannel.as_ref().is_none() {
            return Err(Error::from(E_UNEXPECTED));
        }
        if pbaccept.is_null() {
            return Err(Error::from(E_POINTER));
        }
        unsafe { *pbaccept = BOOL::from(true) };
        let callback: IWTSVirtualChannelCallback =
            AccessKitChannelCallback::new(&self.name).into();
        ppcallback.write(callback.into())?;
        Ok(())
    }
}
