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

/// The channel name the plugin registers a listener for, and the one an RDP
/// server serving accessibility trees opens.
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

/// **The consumer speaks first.** Opening the channel must put the protocol's
/// opening `Hello` on the wire without being asked: the provider is waiting for
/// it, and a plug-in that waited too would deadlock the handshake.
#[test]
fn opening_the_channel_greets_the_provider() {
    let dll = DllHandle::load();
    let plugin = create_plugin(dll);
    let (mgr, state): (IWTSVirtualChannelManager, _) = FakeChannelMgr::new();
    unsafe { plugin.Initialize(&mgr) }.expect("Initialize failed");
    let (_name, listener_cb) =
        state.listeners.lock().first().cloned().expect("no listener was created");
    let (channel, chan_state) = FakeVirtualChannel::new();
    let callback = trigger_new_channel(&listener_cb, &channel);

    let greeting = chan_state.flat_writes();
    assert!(
        !greeting.is_empty(),
        "the plug-in must open the conversation, not wait to be spoken to",
    );
    // Length-prefixed JSON, and the handshake codec is readable, so this is
    // checkable without decoding the protocol. `Message` is tagged `t`, so a
    // Hello is literally `"t":"hello"` on the wire.
    let text = String::from_utf8_lossy(&greeting);
    assert!(
        text.contains("\"hello\""),
        "expected a Hello frame in {} bytes, got {text:?}",
        greeting.len(),
    );

    unsafe { callback.OnClose() }.expect("OnClose failed");
}

/// Garbage on the channel is a protocol error, not a crash: the RDP client's
/// own thread delivers it, and taking that thread down takes the session with
/// it.
#[test]
fn rubbish_on_the_channel_does_not_take_the_session_down() {
    let dll = DllHandle::load();
    let plugin = create_plugin(dll);
    let (mgr, state): (IWTSVirtualChannelManager, _) = FakeChannelMgr::new();
    unsafe { plugin.Initialize(&mgr) }.expect("Initialize failed");
    let (_name, listener_cb) =
        state.listeners.lock().first().cloned().expect("no listener was created");
    let (channel, _chan_state) = FakeVirtualChannel::new();
    let callback = trigger_new_channel(&listener_cb, &channel);

    let payload: &[u8] = b"hello";
    unsafe { callback.OnDataReceived(payload) }.expect("OnDataReceived must not fail");
    unsafe { callback.OnClose() }.expect("OnClose failed");
}
