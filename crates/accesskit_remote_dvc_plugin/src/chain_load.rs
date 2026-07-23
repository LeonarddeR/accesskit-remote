//! Chain-loading the stock WSLg DVC plug-in.
//!
//! When our DLL occupies the single `WSLDVC_PRIVATE` plug-in slot of the
//! `/wslg` msrdc client, that client loads exactly one plug-in from the slot.
//! To keep WSLg's own RAIL app-list integration working, our
//! `VirtualChannelGetInstance` also instantiates the stock `WSLDVCPlugin.dll`
//! and returns its plug-in alongside ours. On the classic-AddIns route the
//! stock plug-in loads itself, so chain-loading must be skipped there.

use core::ffi::c_void;
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::{debug, error, warn};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows::core::{GUID, HRESULT, PCWSTR, s};

const STOCK_DLL_NAME: &str = "WSLDVCPlugin.dll";
const STOCK_DLL_FALLBACK: &str = r"C:\Program Files\WSL\WSLDVCPlugin.dll";
const PRIVATE_SLOT_ARG: &str = "WSLDVC_PRIVATE";

type GetInstanceFn = unsafe extern "system" fn(*const GUID, *mut u32, *mut *mut c_void) -> HRESULT;

/// True iff our DLL occupies the `WSLDVC_PRIVATE` slot, i.e. the host command
/// line names that plug-in. Only then must we chain-load the stock plug-in; on
/// the classic-AddIns route the stock plug-in loads itself.
pub(crate) fn wants_stock_chain_load() -> bool {
    args_request_private_slot(std::env::args_os())
}

fn args_request_private_slot(args: impl Iterator<Item = OsString>) -> bool {
    args.filter_map(|a| a.into_string().ok())
        .any(|a| a.to_ascii_uppercase().contains(PRIVATE_SLOT_ARG))
}

/// Path to the stock WSLg DVC plug-in. Derived from the host process
/// executable directory (msrdc lives in `C:\Program Files\WSL`), falling back
/// to the known absolute path.
pub(crate) fn stock_dll_path() -> PathBuf {
    if let Some(dir) = host_exe_dir() {
        let candidate = dir.join(STOCK_DLL_NAME);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(STOCK_DLL_FALLBACK)
}

fn host_exe_dir() -> Option<PathBuf> {
    let mut buf = vec![0u16; 512];
    loop {
        let len = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
        if len == 0 {
            return None;
        }
        if len < buf.len() {
            let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
            return path.parent().map(|p| p.to_path_buf());
        }
        if buf.len() >= 32768 {
            return None;
        }
        buf.resize(buf.len() * 2, 0);
    }
}

/// The stock plug-in's `VirtualChannelGetInstance`, loaded once and cached. The
/// module handle is intentionally leaked (never `FreeLibrary`d) so the stock
/// code stays mapped for the session.
fn stock_get_instance() -> Option<GetInstanceFn> {
    static FN: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *FN.get_or_init(|| unsafe { load_stock_get_instance() });
    addr.map(|a| unsafe { std::mem::transmute::<usize, GetInstanceFn>(a) })
}

unsafe fn load_stock_get_instance() -> Option<usize> {
    let path = stock_dll_path();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let module = match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
        Ok(m) if !m.is_invalid() => m,
        Ok(_) => {
            error!("LoadLibraryW({path:?}) returned an invalid handle");
            return None;
        }
        Err(e) => {
            error!("LoadLibraryW({path:?}) failed: {e:?}");
            return None;
        }
    };
    // Intentionally leak `module`: keep the stock plug-in mapped for the session.
    match unsafe { GetProcAddress(module, s!("VirtualChannelGetInstance")) } {
        Some(f) => {
            debug!("Chain-loaded stock plug-in from {path:?}");
            Some(f as usize)
        }
        None => {
            error!("stock {path:?} has no VirtualChannelGetInstance export");
            None
        }
    }
}

/// Number of plug-ins the stock DLL implements (0 if unavailable).
pub(crate) fn stock_probe_count(riid: *const GUID) -> u32 {
    let Some(f) = stock_get_instance() else {
        return 0;
    };
    let mut n: u32 = 0;
    let hr = unsafe { f(riid, &mut n, core::ptr::null_mut()) };
    if hr.is_ok() {
        n
    } else {
        warn!("stock probe returned {hr:?}");
        0
    }
}

/// Fill `slots` with stock plug-in pointers; returns the count actually
/// written (never more than `slots.len()`).
pub(crate) unsafe fn stock_fetch(riid: *const GUID, slots: &mut [*mut c_void]) -> u32 {
    if slots.is_empty() {
        return 0;
    }
    let Some(f) = stock_get_instance() else {
        return 0;
    };
    let mut n = slots.len() as u32;
    let hr = unsafe { f(riid, &mut n, slots.as_mut_ptr()) };
    if hr.is_ok() {
        n.min(slots.len() as u32)
    } else {
        warn!("stock fetch returned {hr:?}");
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::RemoteDesktop::IWTSPlugin;
    use windows::core::Interface;

    #[test]
    fn detects_private_slot_arg() {
        let args = ["msrdc.exe", "/wslg", "/plugin:WSLDVC_PRIVATE", "wslg.rdp"]
            .into_iter()
            .map(OsString::from);
        assert!(args_request_private_slot(args));
    }

    #[test]
    fn ignores_package_slot_arg() {
        let args =
            ["mstsc.exe", "/wslg", "/plugin:WSLDVC_PACKAGE"].into_iter().map(OsString::from);
        assert!(!args_request_private_slot(args));
    }

    #[test]
    fn ignores_empty_command_line() {
        let args = std::iter::empty::<OsString>();
        assert!(!args_request_private_slot(args));
    }

    #[test]
    fn stock_dll_path_names_wsldvcplugin() {
        let p = stock_dll_path();
        assert_eq!(p.file_name().unwrap().to_string_lossy(), STOCK_DLL_NAME);
    }

    #[test]
    fn stock_probe_when_present() {
        // Only runs where the WSLg package is installed. Uses the null-array
        // probe path, which the stock plug-in answers without constructing an
        // instance.
        if !stock_dll_path().is_file() {
            eprintln!("stock {STOCK_DLL_NAME} absent; skipping");
            return;
        }
        let n = stock_probe_count(&IWTSPlugin::IID);
        assert!(n >= 1, "stock probe returned {n}");
    }
}
