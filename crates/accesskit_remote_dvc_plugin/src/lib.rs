//! The DLL loaded by the RDP client (msrdc/mstsc) via the dynamic virtual
//! channel add-in mechanism.
//!
//! COM plugin scaffolding (IWTSPlugin and friends) wiring the client core and
//! the UIA host to a transport: an out-of-band socket to the WSL user distro
//! in phase 1, a real DVC once a server-side channel endpoint exists.
#![cfg(target_os = "windows")]
