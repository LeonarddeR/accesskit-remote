//! Smoke tests for the DLL's `VirtualChannelGetInstance` export contract.
//!
//! These load the built cdylib at runtime via libloading. Build it first:
//! `cargo build -p accesskit_remote_dvc_plugin --target x86_64-pc-windows-msvc`.
#![cfg(target_os = "windows")]

mod common;

use common::{DllHandle, create_plugin};
use windows::Win32::Foundation::E_NOINTERFACE;
use windows::Win32::System::RemoteDesktop::IWTSPlugin;
use windows::core::{GUID, Interface};

#[test]
fn export_is_present() {
    // `load()` resolves `VirtualChannelGetInstance`; it panics if the export
    // is missing, so a successful load proves the export exists.
    let _dll = DllHandle::load();
}

#[test]
fn probe_reports_at_least_one_plugin() {
    let dll = DllHandle::load();
    let iid = IWTSPlugin::IID;
    let mut n: u32 = 0;
    let hr = unsafe { (dll.get_instance)(&iid, &mut n, core::ptr::null_mut()) };
    assert!(hr.is_ok(), "probe returned {hr:?}");
    assert!(n >= 1, "probe reported {n} plugins, expected >= 1");
}

#[test]
fn fetch_returns_a_plugin() {
    let dll = DllHandle::load();
    let _plugin: IWTSPlugin = create_plugin(dll);
}

#[test]
fn wrong_iid_returns_no_interface() {
    let dll = DllHandle::load();
    let bad = GUID::from_u128(0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF_DEAD_BEEF);
    let mut n: u32 = 0;
    let hr = unsafe { (dll.get_instance)(&bad, &mut n, core::ptr::null_mut()) };
    assert_eq!(hr, E_NOINTERFACE, "wrong IID should return E_NOINTERFACE, got {hr:?}");
}
