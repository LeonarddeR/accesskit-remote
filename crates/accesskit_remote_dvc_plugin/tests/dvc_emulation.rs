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

/// **The greeting must not go out from inside the accept callback, and must
/// still go out.**
///
/// The RDP client calls `OnNewChannelConnection` while processing the server's
/// Create Request and only sends the Create Response after it returns, so a
/// write issued from inside it reaches a channel the server still has in
/// `Creation` — which MS-RDPEDYC has no state for. The server rejects the data
/// PDU and the whole RDP session goes down with it, which is exactly what
/// happened against a real mstsc: every session died within milliseconds,
/// presenting as the server closing the socket.
///
/// So: nothing on accept, and the handshake shortly afterwards regardless —
/// the provider is waiting for it, and a plug-in that waited forever would
/// deadlock instead.
#[test]
fn the_greeting_waits_for_the_channel_but_still_arrives() {
    let dll = DllHandle::load();
    let plugin = create_plugin(dll);
    let (mgr, state): (IWTSVirtualChannelManager, _) = FakeChannelMgr::new();
    unsafe { plugin.Initialize(&mgr) }.expect("Initialize failed");
    let (_name, listener_cb) =
        state.listeners.lock().first().cloned().expect("no listener was created");
    let (channel, chan_state) = FakeVirtualChannel::new();
    let callback = trigger_new_channel(&listener_cb, &channel);

    assert!(
        chan_state.flat_writes().is_empty(),
        "nothing may be written from inside the accept callback — the channel is not open yet",
    );

    // It still has to arrive, or the provider waits forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while chan_state.flat_writes().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let greeting = chan_state.flat_writes();
    assert!(
        !greeting.is_empty(),
        "the handshake must follow once the channel has had time to open",
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
