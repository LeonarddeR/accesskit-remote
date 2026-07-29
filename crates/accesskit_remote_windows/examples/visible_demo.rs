//! Demonstrates UIA hosting on an already-visible window: the window is shown
//! first, then `install_visible_adapter` attaches — the case
//! `SubclassingAdapter` refuses. The tree comes from a real in-process
//! provider↔consumer pair, so UIA activation exercises the genuine
//! `ClientConnection::snapshot` path; button clicks round-trip through the
//! action channel back to the provider, whose label update returns as a
//! posted delta.
//!
//! Verify with a UIA client (e.g. System.Windows.Automation in Windows
//! PowerShell): find the "AccessKit Visible Demo" window, read the tree,
//! invoke the button, re-read the label.
#![cfg(target_os = "windows")]

use accesskit_remote::{AppInfo, WindowId};
use accesskit_remote_client::{ClientConnection, ClientEvent};
use accesskit_remote_server::{ServerConnection, ServerEvent, WindowDescriptor};
use accesskit_remote_windows::{SharedClient, install_visible_adapter, post_delta};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG,
    PostQuitMessage, RegisterClassW, SW_SHOW, SetTimer, ShowWindow, TranslateMessage, WM_DESTROY,
    WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::w;

const WINDOW: WindowId = WindowId(1);
const ROOT: accesskit::NodeId = accesskit::NodeId(0);
const LABEL: accesskit::NodeId = accesskit::NodeId(1);
const BUTTON: accesskit::NodeId = accesskit::NodeId(2);
const DOC: accesskit::NodeId = accesskit::NodeId(3);
const RUN0: accesskit::NodeId = accesskit::NodeId(4);
const RUN1: accesskit::NodeId = accesskit::NodeId(5);

/// Builds a Role::TextRun node with per-code-point character lengths.
fn run(value: &str) -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::TextRun);
    node.set_value(value.to_owned());
    node.set_character_lengths(value.chars().map(|c| c.len_utf8() as u8).collect::<Vec<u8>>());
    node
}

/// The document container with a caret at (`caret_run`, `caret_index`). The
/// selection references run nodes that need not be present in the same update.
fn doc_node(caret_run: accesskit::NodeId, caret_index: usize) -> accesskit::Node {
    let mut doc = accesskit::Node::new(accesskit::Role::MultilineTextInput);
    doc.set_children(vec![RUN0, RUN1]);
    let pos = accesskit::TextPosition { node: caret_run, character_index: caret_index };
    doc.set_text_selection(accesskit::TextSelection { anchor: pos, focus: pos });
    doc
}

/// In-process provider: the demo tree plus click handling.
struct Provider {
    server: ServerConnection,
    clicks: u32,
}

impl Provider {
    fn new() -> Self {
        Self { server: ServerConnection::new("visible_demo_provider"), clicks: 0 }
    }

    fn label_node(&self) -> accesskit::Node {
        let mut label = accesskit::Node::new(accesskit::Role::Label);
        label.set_value(format!("Button clicked {} times", self.clicks));
        label.add_action(accesskit::Action::Focus);
        label
    }

    fn full_tree(&self) -> accesskit::TreeUpdate {
        let mut root = accesskit::Node::new(accesskit::Role::Window);
        root.set_label("AccessKit Visible Demo");
        root.set_children(vec![LABEL, BUTTON, DOC]);
        let mut button = accesskit::Node::new(accesskit::Role::Button);
        button.set_label("Click me");
        button.add_action(accesskit::Action::Click);
        button.add_action(accesskit::Action::Focus);
        accesskit::TreeUpdate {
            nodes: vec![
                (ROOT, root),
                (LABEL, self.label_node()),
                (BUTTON, button),
                (DOC, doc_node(RUN0, 0)),
                (RUN0, run("Hello\n")),
                (RUN1, run("World")),
            ],
            tree: Some(accesskit::Tree::new(ROOT)),
            tree_id: accesskit::TreeId::ROOT,
            focus: BUTTON,
        }
    }

    fn descriptor(&self) -> WindowDescriptor {
        WindowDescriptor {
            id: WINDOW,
            title: "AccessKit Visible Demo".into(),
            app: AppInfo {
                name: "visible_demo".into(),
                app_id: Some("dev.accesskit.VisibleDemo".into()),
                pid: Some(std::process::id()),
                toolkit: Some("visible_demo".into()),
                toolkit_version: None,
            },
            native_window_id: None,
        }
    }

    fn perform(&mut self, request: &accesskit::ActionRequest) {
        match request.action {
            accesskit::Action::Click if request.target_node == BUTTON => {
                self.clicks += 1;
                let update = accesskit::TreeUpdate {
                    nodes: vec![(LABEL, self.label_node())],
                    tree: None,
                    tree_id: accesskit::TreeId::ROOT,
                    focus: BUTTON,
                };
                self.server.send_tree_update(WINDOW, update).expect("send_tree_update");
                // A caret-only, container-only delta: only DOC is present, and
                // its selection references RUN0/RUN1, which are not in the
                // update. Alternate the caret start↔end.
                let (caret_run, caret_index) =
                    if self.clicks % 2 == 1 { (RUN1, 5) } else { (RUN0, 0) };
                let caret = accesskit::TreeUpdate {
                    nodes: vec![(DOC, doc_node(caret_run, caret_index))],
                    tree: None,
                    tree_id: accesskit::TreeId::ROOT,
                    focus: BUTTON,
                };
                self.server.send_tree_update(WINDOW, caret).expect("send_tree_update caret");
            }
            // A focus-only delta: no nodes, just the new focus. Exercises the
            // path node focus forwarding relies on.
            accesskit::Action::Focus => {
                let update = accesskit::TreeUpdate {
                    nodes: Vec::new(),
                    tree: None,
                    tree_id: accesskit::TreeId::ROOT,
                    focus: request.target_node,
                };
                self.server.send_tree_update(WINDOW, update).expect("send_tree_update");
            }
            _ => {}
        }
    }
}

/// Move bytes both ways until both sides go quiet; returns the client events.
fn shuttle(provider: &mut Provider, client: &SharedClient) -> Vec<ClientEvent> {
    let mut all = Vec::new();
    loop {
        let to_client = provider.server.take_output();
        let to_server = client.lock().unwrap().take_output();
        if to_client.is_empty() && to_server.is_empty() {
            break;
        }
        if !to_client.is_empty() {
            all.extend(client.lock().unwrap().handle_input(&to_client).expect("client input"));
        }
        if !to_server.is_empty() {
            for event in provider.server.handle_input(&to_server).expect("server input") {
                if let ServerEvent::Established { .. } = event {
                    let descriptor = provider.descriptor();
                    let tree = provider.full_tree();
                    provider
                        .server
                        .sync_initial_state(vec![(descriptor, tree)], Some(WINDOW))
                        .expect("sync_initial_state");
                }
            }
        }
    }
    all
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn main() {
    // 1. Establish the in-process provider↔consumer session.
    let mut provider = Provider::new();
    let client: SharedClient =
        Arc::new(Mutex::new(ClientConnection::new("visible_demo_client")));
    let events = shuttle(&mut provider, &client);
    eprintln!("visible_demo: session events: {events:?}");
    assert!(client.lock().unwrap().windows().next().is_some(), "no window synced");

    // 2. Create AND SHOW the window before any adapter exists.
    let instance = unsafe { GetModuleHandleW(None) }.unwrap();
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: w!("AccessKitVisibleDemo"),
        ..Default::default()
    };
    assert_ne!(unsafe { RegisterClassW(&class) }, 0);
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("AccessKitVisibleDemo"),
            w!("AccessKit Visible Demo"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            400,
            300,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .unwrap();
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    eprintln!("visible_demo: window {hwnd:?} shown; attaching adapter to the visible window");

    // 3. Attach to the already-visible window.
    let (action_tx, action_rx) = mpsc::channel();
    install_visible_adapter(hwnd, WINDOW, client.clone(), action_tx, true)
        .expect("install_visible_adapter");
    eprintln!("visible_demo: adapter installed");

    // 4. Pump: drain UIA actions on a timer tick, round-trip through the
    //    provider, and post resulting deltas back to the window.
    unsafe { SetTimer(Some(hwnd), 1, 100, None) };
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        while let Ok((window, request)) = action_rx.try_recv() {
            eprintln!("visible_demo: action on window {}: {request:?}", window.0);
            provider.perform(&request);
            for event in shuttle(&mut provider, &client) {
                if let ClientEvent::TreeUpdated { window, update } = event {
                    eprintln!("visible_demo: posting delta for window {}", window.0);
                    post_delta(hwnd, update);
                }
            }
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
