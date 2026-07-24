//! Self-registration: writing the HKCU `OptionalAddIns\WSLDVC_PRIVATE` entry
//! (and the `.wslgconfig` flag) so `regsvr32 <dll>` installs the plug-in with no
//! elevation, and `regsvr32 /u <dll>` removes both. Just the DLL path — no COM
//! CLSID / `InprocServer32` class registration (the plug-in loads via the
//! instance-method entry point, not a class factory).

use std::path::PathBuf;
use tracing::{debug, error};
use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::core::HRESULT;

use crate::wslgconfig;

const OPTIONAL_ADDINS_WSLDVC_PRIVATE: &str =
    r"Software\Microsoft\Terminal Server Client\Default\OptionalAddIns\WSLDVC_PRIVATE";
const NAME_VALUE: &str = "Name";

/// This DLL's own filesystem path, resolved from the module handle captured in
/// `DllMain`.
fn module_path() -> Option<PathBuf> {
    let mut buf = vec![0u16; 512];
    loop {
        let len = unsafe { GetModuleFileNameW(Some(crate::instance()), &mut buf) } as usize;
        if len == 0 {
            return None;
        }
        if len < buf.len() {
            return Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        if buf.len() >= 32768 {
            return None;
        }
        buf.resize(buf.len() * 2, 0);
    }
}

pub fn register() -> HRESULT {
    let Some(path) = module_path() else {
        error!("register: could not resolve module path");
        return E_FAIL;
    };
    let path = path.to_string_lossy().into_owned();
    let write = windows_registry::CURRENT_USER
        .create(OPTIONAL_ADDINS_WSLDVC_PRIVATE)
        .and_then(|k| k.set_string(NAME_VALUE, &path));
    if let Err(e) = write {
        error!("register: registry write failed: {e:?}");
        return E_FAIL;
    }
    debug!("register: HKCU OptionalAddIns\\WSLDVC_PRIVATE Name={path}");
    if let Err(e) = wslgconfig::install() {
        error!("register: .wslgconfig update failed: {e:?}");
        return E_FAIL;
    }
    S_OK
}

pub fn unregister() -> HRESULT {
    if let Err(e) = windows_registry::CURRENT_USER.remove_tree(OPTIONAL_ADDINS_WSLDVC_PRIVATE) {
        debug!("unregister: remove_tree returned {e:?} (ok if key absent)");
    }
    if let Err(e) = wslgconfig::uninstall() {
        error!("unregister: .wslgconfig update failed: {e:?}");
        return E_FAIL;
    }
    S_OK
}
