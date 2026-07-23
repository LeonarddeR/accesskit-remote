//! The DLL loaded by the RDP client (msrdc/mstsc) via the dynamic virtual
//! channel add-in mechanism.
//!
//! COM plugin scaffolding (IWTSPlugin and friends) wiring the client core and
//! the UIA host to a transport: an out-of-band socket to the WSL user distro
//! in phase 1, a real DVC once a server-side channel endpoint exists.
//!
//! The DLL is loaded via the instance method: it exports
//! [`VirtualChannelGetInstance`], which the RDC client calls by name to obtain
//! the [`IWTSPlugin`] instances the DLL implements.
#![cfg(target_os = "windows")]

pub mod association;
mod chain_load;
mod channel;
mod listener;
mod plugin;
mod rail;
pub mod transport;

use core::ffi::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::str::FromStr;
use std::sync::atomic::{AtomicIsize, Ordering};
use tracing::{debug, error, trace, warn};
use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, E_UNEXPECTED, HMODULE, S_OK};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::RemoteDesktop::IWTSPlugin;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows::core::{GUID, HRESULT, Interface};
use windows_core::BOOL;

use plugin::AccessKitDvcPlugin;

static INSTANCE: AtomicIsize = AtomicIsize::new(0);

/// The DLL's own module handle, captured in `DllMain`.
pub(crate) fn instance() -> HMODULE {
    HMODULE(INSTANCE.load(Ordering::Acquire) as _)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            INSTANCE.store(hinst.0 as _, Ordering::Release);
            let file_appender =
                tracing_appender::rolling::never(std::env::temp_dir(), "AccessKitDvc.log");
            let log_level = std::env::var("ACCESSKIT_DVC_LOG")
                .ok()
                .and_then(|s| tracing::Level::from_str(&s).ok())
                .unwrap_or(tracing::Level::DEBUG);
            tracing_subscriber::fmt()
                .compact()
                .with_writer(file_appender)
                .with_ansi(false)
                .with_max_level(log_level)
                .init();
            panic::set_hook(Box::new(|info| {
                error!("{:?}", info);
            }));
            trace!(
                "DllMain: DLL_PROCESS_ATTACH, arch {}, log level {log_level}",
                std::env::consts::ARCH
            );
            match unsafe { DisableThreadLibraryCalls(hinst) } {
                Ok(_) => trace!("Disabled thread library calls"),
                Err(e) => {
                    error!("Failed to disable thread library calls: {e:?}");
                    return false.into();
                }
            }
        }
        DLL_PROCESS_DETACH => {
            debug!("DllMain: DLL_PROCESS_DETACH");
        }
        _ => {}
    }
    true.into()
}

/// The DVC instance entry point. Exported by name so the RDC client can
/// `LoadLibrary` + `GetProcAddress` it (see [DVC plug-in registration]).
///
/// Two-call contract: with `ppo` null, writes the number of plug-ins into
/// `*pnumobjs` (probe); otherwise fills `ppo` with that many `IWTSPlugin`
/// pointers and reports the count actually written.
///
/// # Safety
/// Called by the RDC client with pointers per the DVC contract.
///
/// [DVC plug-in registration]: https://learn.microsoft.com/windows/win32/termserv/dvc-plug-in-registration
#[unsafe(no_mangle)]
pub unsafe extern "system" fn VirtualChannelGetInstance(
    refiid: *const GUID,
    pnumobjs: *mut u32,
    ppo: *mut *mut c_void,
) -> HRESULT {
    // A Rust panic must never unwind across the FFI boundary.
    match panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        get_instance(refiid, pnumobjs, ppo)
    })) {
        Ok(hr) => hr,
        Err(_) => {
            error!("VirtualChannelGetInstance panicked");
            E_UNEXPECTED
        }
    }
}

unsafe fn get_instance(refiid: *const GUID, pnumobjs: *mut u32, ppo: *mut *mut c_void) -> HRESULT {
    debug!("VirtualChannelGetInstance called (ppObjArray null = {})", ppo.is_null());
    if refiid.is_null() || pnumobjs.is_null() {
        return E_POINTER;
    }
    let riid = unsafe { *refiid };
    if riid != IWTSPlugin::IID {
        error!("VirtualChannelGetInstance: unknown interface {riid:?}");
        return E_NOINTERFACE;
    }

    // Chain-load the stock plug-in only when we occupy the WSLDVC_PRIVATE slot;
    // on the classic-AddIns route the stock plug-in loads itself.
    let chain = chain_load::wants_stock_chain_load();
    let stock_n = if chain { chain_load::stock_probe_count(refiid) } else { 0 };
    let pnumobjs = unsafe { &mut *pnumobjs };

    if ppo.is_null() {
        // Probe: report the maximum number of plug-ins we may return.
        *pnumobjs = 1 + stock_n;
        debug!(
            "VirtualChannelGetInstance probe: reporting {} (ours=1, stock={stock_n}, chain={chain})",
            *pnumobjs
        );
        return S_OK;
    }

    // Fetch: never write past the array the caller allocated.
    let cap = *pnumobjs as usize;
    debug!("VirtualChannelGetInstance fetch: caller array cap = {cap} (chain={chain}, stock={stock_n})");
    if cap < 1 {
        error!("VirtualChannelGetInstance fetch: caller array too small ({cap})");
        *pnumobjs = 0;
        return S_OK;
    }
    let slots = unsafe { std::slice::from_raw_parts_mut(ppo, cap) };
    let mut written = 0usize;

    // Stock plug-in(s) first, into the remaining slots.
    if chain && written < cap {
        written += unsafe { chain_load::stock_fetch(refiid, &mut slots[written..]) } as usize;
    }

    // Our plug-in last, if room remains.
    if written < cap {
        let plugin: IWTSPlugin = AccessKitDvcPlugin::new().into();
        slots[written] = plugin.into_raw();
        written += 1;
    } else {
        warn!("VirtualChannelGetInstance fetch: no room for our plug-in (cap={cap}); stock only");
    }

    *pnumobjs = written as u32;
    debug!("VirtualChannelGetInstance fetch: wrote {written} plugin(s)");
    S_OK
}
