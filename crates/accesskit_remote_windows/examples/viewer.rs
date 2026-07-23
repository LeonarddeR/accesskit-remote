//! Standalone viewer: connects to a provider and gives every remote window
//! a native window whose UIA tree is the remote AccessKit tree. Lets a
//! Windows screen reader read a provider (e.g. the accesskit_remoted demo,
//! or later real Linux apps) without the RDP client.
//!
//! Usage:
//!   viewer --tcp [PORT]
//!   viewer --hvsocket <vm-id> [PORT]

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
    use accesskit_remote_windows::{OutgoingAction, RemoteWindowBinding, SharedClient};
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

    pub fn run() -> std::io::Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut stream = connect(&args)?;
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;

        let client: SharedClient = Arc::new(Mutex::new(ClientConnection::new("viewer")));
        let (action_tx, action_rx) = mpsc::channel::<OutgoingAction>();
        let ui_thread = unsafe { GetCurrentThreadId() };

        let pump_client = client.clone();
        std::thread::spawn(move || pump(stream, pump_client, action_rx, ui_thread));

        run_message_loop(client, action_tx);
        Ok(())
    }

    fn connect(args: &[String]) -> std::io::Result<accesskit_remote_transport::Socket> {
        match args.first().map(String::as_str) {
            Some("--tcp") | None => {
                let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(4750);
                accesskit_remote_transport::tcp::connect_local(port).map(Into::into)
            }
            Some("--hvsocket") => {
                let vm_id = args
                    .get(1)
                    .expect("usage: viewer --hvsocket <vm-id> [PORT]")
                    .parse::<uuid::Uuid>()
                    .expect("invalid VM ID");
                let port: u32 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(4750);
                accesskit_remote_transport::hvsocket::connect(vm_id, port)
            }
            Some(other) => Err(std::io::Error::other(format!("unknown mode: {other}"))),
        }
    }

    fn pump(
        mut stream: accesskit_remote_transport::Socket,
        client: SharedClient,
        actions: mpsc::Receiver<OutgoingAction>,
        ui_thread: u32,
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
            let events = match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => match client.lock().unwrap().handle_input(&buf[..n]) {
                    Ok(events) => events,
                    Err(e) => {
                        eprintln!("viewer: protocol error: {e}");
                        break;
                    }
                },
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
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
