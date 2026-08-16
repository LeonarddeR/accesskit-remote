//! Standalone viewer: connects to a provider and shows its trees to a Windows
//! screen reader without involving the RDP client at all.
//!
//! Two shapes, matching the two arrangements this project has to serve:
//!
//! - **Window mode** (default): one native window per remote window, each
//!   carrying that window's tree. This mirrors RAIL, where the RDP client
//!   really does give every remote window a local HWND.
//! - **Desktop mode** (`--desktop`): *one* window carrying the whole remote
//!   desktop, every remote window grafted into it as a subtree. This mirrors a
//!   full-desktop RDP session — macrdp — where there is one session window and
//!   nothing per-window to attach to.
//!
//! Desktop mode exists to be testable here: the composition it exercises is the
//! same one the DVC plug-in uses, minus RDP, so it can be read with a real
//! screen reader over nothing but an SSH tunnel.
//!
//! Usage:
//!   viewer [--desktop] --tcp [PORT]
//!   viewer [--desktop] --hvsocket <vm-id> [PORT]

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    viewer::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("viewer runs on Windows only");
}

#[cfg(windows)]
mod viewer {
    use accesskit_remote::WindowId;
    use accesskit_remote_client::{ClientConnection, ClientEvent};
    use accesskit_remote_windows::{
        DesktopBinding, OutgoingAction, RemoteWindowBinding, SharedClient,
    };
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostThreadMessageW, RegisterClassW, ShowWindow, TranslateMessage, CW_USEDEFAULT, MSG,
        SW_SHOWNORMAL, WINDOW_EX_STYLE, WM_APP, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };

    const MSG_WINDOW_ADDED: u32 = WM_APP + 1;
    const MSG_TREE_UPDATED: u32 = WM_APP + 2;
    const MSG_WINDOW_REMOVED: u32 = WM_APP + 3;
    const MSG_CONNECTION_CLOSED: u32 = WM_APP + 4;
    /// Desktop mode's heartbeat: the composed tree is rebuilt on activation,
    /// which can happen at any moment with no client event to notice it.
    const MSG_SYNC: u32 = WM_APP + 5;

    pub fn run() -> std::io::Result<()> {
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        let desktop = args.iter().any(|a| a == "--desktop");
        args.retain(|a| a != "--desktop");
        let (stream, hvsocket) = connect(&args)?;
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;

        let client: SharedClient = Arc::new(Mutex::new(ClientConnection::new("viewer")));
        let (action_tx, action_rx) = mpsc::channel::<OutgoingAction>();
        let ui_thread = unsafe { GetCurrentThreadId() };

        let pump_client = client.clone();
        std::thread::spawn(move || pump(stream, pump_client, action_rx, ui_thread, hvsocket));

        if desktop {
            run_desktop_loop(client, action_tx);
        } else {
            run_message_loop(client, action_tx);
        }
        Ok(())
    }

    /// Connects and reports whether the transport is hvsocket, whose read
    /// timeout surfaces as `ConnectionAborted` rather than
    /// `WouldBlock`/`TimedOut`.
    fn connect(args: &[String]) -> std::io::Result<(accesskit_remote_transport::Socket, bool)> {
        match args.first().map(String::as_str) {
            Some("--tcp") | None => {
                let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(4750);
                let socket = accesskit_remote_transport::tcp::connect_local(port)?;
                Ok((socket.into(), false))
            }
            Some("--hvsocket") => {
                let vm_id = args
                    .get(1)
                    .expect("usage: viewer --hvsocket <vm-id> [PORT]")
                    .parse::<uuid::Uuid>()
                    .expect("invalid VM ID");
                let port: u32 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(4750);
                Ok((accesskit_remote_transport::hvsocket::connect(vm_id, port)?, true))
            }
            Some(other) => Err(std::io::Error::other(format!("unknown mode: {other}"))),
        }
    }

    /// Whether a read error is a timeout to retry rather than a dead
    /// connection. hvsocket surfaces receive timeouts as `ConnectionAborted`.
    fn is_retryable_read(err: &std::io::Error, hvsocket: bool) -> bool {
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) || (hvsocket && err.kind() == std::io::ErrorKind::ConnectionAborted)
    }

    fn pump(
        mut stream: accesskit_remote_transport::Socket,
        client: SharedClient,
        actions: mpsc::Receiver<OutgoingAction>,
        ui_thread: u32,
        hvsocket: bool,
    ) {
        let post = |message: u32, wparam: usize, lparam: isize| unsafe {
            let _ = PostThreadMessageW(ui_thread, message, WPARAM(wparam), LPARAM(lparam));
        };
        let mut buf = [0u8; 16384];
        loop {
            while let Ok((window, request)) = actions.try_recv() {
                if let Err(e) = client.lock().unwrap().request_action(window, request) {
                    eprintln!("viewer: action failed: {e}");
                }
            }
            let out = client.lock().unwrap().take_output();
            if !out.is_empty() {
                if stream.write_all(&out).is_err() {
                    break;
                }
            }
            // Cheap and idempotent; desktop mode needs it because a screen
            // reader can activate the adapter with no client event in sight.
            post(MSG_SYNC, 0, 0);
            let events = match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => match client.lock().unwrap().handle_input(&buf[..n]) {
                    Ok(events) => events,
                    Err(e) => {
                        eprintln!("viewer: protocol error: {e}");
                        break;
                    }
                },
                Err(e) if is_retryable_read(&e, hvsocket) => continue,
                Err(_) => break,
            };
            for event in events {
                match event {
                    ClientEvent::Connected => eprintln!("viewer: connected"),
                    ClientEvent::WindowAdded { window } => post(MSG_WINDOW_ADDED, window.0 as usize, 0),
                    ClientEvent::TreeUpdated { window, update } => {
                        let raw = Box::into_raw(Box::new(update));
                        post(MSG_TREE_UPDATED, window.0 as usize, raw as isize);
                    }
                    ClientEvent::WindowRemoved { window } => {
                        post(MSG_WINDOW_REMOVED, window.0 as usize, 0)
                    }
                    ClientEvent::FocusChanged { window } => {
                        eprintln!("viewer: focused window: {:?}", window.map(|w| w.0));
                    }
                    ClientEvent::Pong { .. } => {}
                    ClientEvent::Closed { reason } => {
                        eprintln!("viewer: closed: {reason}");
                        post(MSG_CONNECTION_CLOSED, 0, 0);
                        return;
                    }
                }
            }
        }
        post(MSG_CONNECTION_CLOSED, 0, 0);
    }

    /// One window for the whole desktop, as a full-desktop RDP session has.
    ///
    /// The window is created and shown before the binding attaches, matching
    /// the plug-in's situation: there, the session window already exists and
    /// belongs to the RDP client. Everything after that — composition,
    /// activation, action routing — is the code path the plug-in runs.
    fn run_desktop_loop(client: SharedClient, action_tx: mpsc::Sender<OutgoingAction>) {
        let class_name: Vec<u16> = "AccessKitRemoteDesktopWindow\0".encode_utf16().collect();
        let hinstance = unsafe { GetModuleHandleW(None) }.expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 as isize + 1) as *mut core::ffi::c_void),
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&class) }, 0, "class registration failed");

        let title_w: Vec<u16> = "Remote desktop\0".encode_utf16().collect();
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                800,
                600,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .expect("window creation failed");

        let mut binding =
            DesktopBinding::attach(hwnd, "Remote desktop", client, action_tx);
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNORMAL) };
        eprintln!("viewer: desktop mode; one window carries every remote window");

        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.hwnd.is_invalid() {
                match msg.message {
                    // Window arrivals and departures change the composition, so
                    // both are just a sync.
                    MSG_WINDOW_ADDED | MSG_WINDOW_REMOVED | MSG_SYNC => binding.sync(),
                    MSG_TREE_UPDATED => {
                        let id = msg.wParam.0 as u64;
                        let update = *unsafe {
                            Box::from_raw(msg.lParam.0 as *mut accesskit::TreeUpdate)
                        };
                        // A window's first tree makes it graftable, so take the
                        // delta first and let the sync that follows pick up any
                        // window this was the first update for.
                        binding.delta(WindowId(id), update);
                        binding.sync();
                    }
                    MSG_CONNECTION_CLOSED => break,
                    _ => {}
                }
                continue;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // Remove the subclass before the window it is attached to goes away.
        drop(binding);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn run_message_loop(client: SharedClient, action_tx: mpsc::Sender<OutgoingAction>) {
        let class_name: Vec<u16> = "AccessKitRemoteViewerWindow\0".encode_utf16().collect();
        let hinstance = unsafe { GetModuleHandleW(None) }.expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 as isize + 1) as *mut core::ffi::c_void),
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&class) }, 0, "class registration failed");

        let mut windows_map: HashMap<u64, (HWND, RemoteWindowBinding)> = HashMap::new();
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.hwnd.is_invalid() {
                match msg.message {
                    MSG_WINDOW_ADDED => {
                        let id = msg.wParam.0 as u64;
                        let title = client
                            .lock()
                            .unwrap()
                            .window_info(WindowId(id))
                            .map(|info| info.title.clone())
                            .unwrap_or_else(|| format!("Remote window {id}"));
                        let title_w: Vec<u16> = format!("{title}\0").encode_utf16().collect();
                        let hwnd = unsafe {
                            CreateWindowExW(
                                WINDOW_EX_STYLE(0),
                                PCWSTR(class_name.as_ptr()),
                                PCWSTR(title_w.as_ptr()),
                                WS_OVERLAPPEDWINDOW,
                                CW_USEDEFAULT,
                                CW_USEDEFAULT,
                                600,
                                400,
                                None,
                                None,
                                Some(hinstance.into()),
                                None,
                            )
                        }
                        .expect("window creation failed");
                        let binding = RemoteWindowBinding::attach(
                            hwnd,
                            WindowId(id),
                            client.clone(),
                            action_tx.clone(),
                        );
                        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNORMAL) };
                        eprintln!("viewer: created window for remote {id}: '{title}'");
                        windows_map.insert(id, (hwnd, binding));
                    }
                    MSG_TREE_UPDATED => {
                        let id = msg.wParam.0 as u64;
                        let update = *unsafe {
                            Box::from_raw(msg.lParam.0 as *mut accesskit::TreeUpdate)
                        };
                        if let Some((_, binding)) = windows_map.get_mut(&id) {
                            binding.apply(update);
                        }
                    }
                    MSG_WINDOW_REMOVED => {
                        let id = msg.wParam.0 as u64;
                        if let Some((hwnd, binding)) = windows_map.remove(&id) {
                            drop(binding);
                            let _ = unsafe { DestroyWindow(hwnd) };
                        }
                    }
                    MSG_CONNECTION_CLOSED => break,
                    _ => {}
                }
                continue;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
