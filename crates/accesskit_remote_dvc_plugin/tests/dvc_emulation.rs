//! Plugin lifecycle tests driven through the fake DVC framework objects.
//!
//! Build the cdylib first:
//! `cargo build -p accesskit_remote_dvc_plugin --target x86_64-pc-windows-msvc`.
#![cfg(target_os = "windows")]

mod common;

use common::{
    DllHandle, FakeChannelMgr, FakeVirtualChannel, MgrEvent, create_plugin, trigger_new_channel,
};
use windows::Win32::System::RemoteDesktop::IWTSVirtualChannelManager;

/// The channel name the plugin registers a listener for (phase-1 placeholder).
const CHANNEL_NAME: &str = "AccessKit";

#[test]
fn initialize_creates_listener() {
    let dll = DllHandle::load();
    let plugin = create_plugin(dll);
    let (mgr, state): (IWTSVirtualChannelManager, _) = FakeChannelMgr::new();

    unsafe { plugin.Initialize(&mgr) }.expect("Initialize failed");

    let events = state.events.lock().clone();
    let names: Vec<String> = events
        .into_iter()
        .map(|MgrEvent::CreateListener { name }| name)
        .collect();
    assert!(
        names.iter().any(|n| n == CHANNEL_NAME),
        "expected a CreateListener for {CHANNEL_NAME:?}, got {names:?}"
    );
}

#[test]
fn new_channel_connection_returns_callback() {
    let dll = DllHandle::load();
    let plugin = create_plugin(dll);
    let (mgr, state): (IWTSVirtualChannelManager, _) = FakeChannelMgr::new();
    unsafe { plugin.Initialize(&mgr) }.expect("Initialize failed");

    let (_name, listener_cb) =
        state.listeners.lock().first().cloned().expect("no listener was created");
    let (channel, _chan_state) = FakeVirtualChannel::new();

    // Returns a callback and sets accept = true (asserted inside the helper).
    let _callback = trigger_new_channel(&listener_cb, &channel);
}

#[test]
fn channel_callback_stub_is_ok() {
    let dll = DllHandle::load();
    let plugin = create_plugin(dll);
    let (mgr, state): (IWTSVirtualChannelManager, _) = FakeChannelMgr::new();
    unsafe { plugin.Initialize(&mgr) }.expect("Initialize failed");
    let (_name, listener_cb) =
        state.listeners.lock().first().cloned().expect("no listener was created");
    let (channel, _chan_state) = FakeVirtualChannel::new();
    let callback = trigger_new_channel(&listener_cb, &channel);

    let payload: &[u8] = b"hello";
    unsafe { callback.OnDataReceived(payload) }
        .expect("OnDataReceived should succeed for the stub");
    unsafe { callback.OnClose() }.expect("OnClose should succeed for the stub");
}
