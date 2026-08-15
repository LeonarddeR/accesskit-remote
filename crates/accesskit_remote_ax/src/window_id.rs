//! The native window id carried on the wire as `nativeWindowId`.
//!
//! The protocol already has a slot for the provider's own window-system id
//! (`Message::WindowAdded::native_window_id`), which on WSLg the daemon fills
//! by tailing Weston's log and joining on app id — 744 lines of ledger,
//! freshness windows and FIFO matching, because the AT-SPI bus knows nothing
//! about the window system.
//!
//! On macOS the answer is one call. `_AXUIElementGetWindow` maps a window
//! element straight to its `CGWindowID`. The catch is that it is private SPI:
//! it has been present and stable for many macOS releases and is what every
//! window manager uses, but it is not API, and it does not resolve in a
//! sandboxed process.
//!
//! So it is resolved at runtime with `dlsym` rather than linked. A missing
//! symbol degrades to `None`, which the wire already models as a supported
//! outcome — a consumer that cannot get a native id falls back to matching on
//! title and app identity, exactly as it does for a provider that never sends
//! one.

use objc2_application_services::{AXError, AXUIElement};
use std::ffi::c_void;
use std::sync::OnceLock;

/// `AXError _AXUIElementGetWindow(AXUIElementRef, CGWindowID *out)`
type GetWindowFn = unsafe extern "C" fn(*const AXUIElement, *mut u32) -> AXError;

/// Resolves the SPI once per process, caching the outcome including failure.
///
/// The address is cached as a `usize` because a function pointer is not `Sync`;
/// it is transmuted back at the call site, which is sound because the symbol's
/// address is stable for the process's lifetime.
fn get_window_fn() -> Option<GetWindowFn> {
    static ADDRESS: OnceLock<Option<usize>> = OnceLock::new();
    let address = (*ADDRESS.get_or_init(|| {
        let name = c"_AXUIElementGetWindow";
        // SAFETY: a valid NUL-terminated name; RTLD_DEFAULT searches images
        // already loaded, so this neither loads nor runs any new code.
        let symbol: *mut c_void = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        if symbol.is_null() {
            tracing::info!(
                "_AXUIElementGetWindow is unavailable; window ids will not be reported"
            );
            None
        } else {
            Some(symbol as usize)
        }
    }))?;
    // SAFETY: the address came from dlsym for this exact symbol, and the
    // signature matches the one every known caller of this SPI uses.
    Some(unsafe { std::mem::transmute::<usize, GetWindowFn>(address) })
}

/// Whether the SPI resolved in this process. Reported by the probe, since
/// "no window ids anywhere" and "no window ids for this app" have very
/// different causes.
pub fn is_available() -> bool {
    get_window_fn().is_some()
}

/// The `CGWindowID` of a window element, if it can be determined.
///
/// `None` covers every failure — unresolved symbol, a non-window element, a
/// dead element — because none of them is recoverable here and the wire treats
/// an absent id as normal.
pub fn window_id(element: &AXUIElement) -> Option<u64> {
    let get_window = get_window_fn()?;
    let mut id: u32 = 0;
    // SAFETY: `element` is a live AXUIElement and `id` is a valid writable u32,
    // which is CGWindowID's layout. The callee writes only on success.
    let error = unsafe { get_window(element, &mut id) };
    // A zero id means "no window", which some elements legitimately report
    // alongside success; it would be a false match on the consumer side.
    (error == AXError::Success && id != 0).then_some(id as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_is_attempted_and_answers_consistently() {
        // Whether the SPI exists is a property of the OS, not something to
        // assert — but resolving it must be total, must not crash, and must
        // give the same answer every time (the result is cached).
        let first = is_available();
        assert_eq!(first, is_available());
        assert_eq!(first, get_window_fn().is_some());
    }

    #[test]
    fn the_system_wide_element_has_no_window_id() {
        // SAFETY: takes no arguments and always returns a valid element.
        let system_wide = unsafe { AXUIElement::new_system_wide() };
        // Not a window, so however the SPI answers, the result must be None
        // rather than a plausible-looking id that would mis-associate on the
        // consumer side.
        assert_eq!(window_id(&system_wide), None);
    }
}
