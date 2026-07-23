//! `IWTSPlugin` implementation: registers a DVC listener when the RDC client
//! initializes the plug-in, and runs the out-of-band hvsocket client for the
//! lifetime of the RDP connection.

use accesskit_remote_client::{ClientConnection, ClientEvent};
use accesskit_remote_windows::SharedClient;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::E_UNEXPECTED;
use windows::Win32::System::RemoteDesktop::{
    IWTSListener, IWTSListenerCallback, IWTSPlugin, IWTSPlugin_Impl, IWTSVirtualChannelManager,
};
use windows::core::{Error, PCSTR, Result, implement};
use windows_core::Ref;

use crate::listener::AccessKitListenerCallback;
use crate::rail::{self, RailHook, RailShared, Registry};
use crate::transport::{self, PumpHandle};

/// The channel name the plug-in listens on. Phase-1 placeholder: in `/wslg`
/// there is no server-side AccessKit DVC channel, so the listener never
/// receives a connection — it exists to prove the plug-in is driven by the
/// client. Distinct from the stock `Microsoft::Windows::RDS::RemoteApplicationList`.
pub const CHANNEL_NAME: &str = "AccessKit";

/// Per-RDP-connection state: the shared client store, the hvsocket pump, the
/// RAIL hook, and the action channel bindings send UIA actions into.
struct Session {
    pump: PumpHandle,
    hook: Option<RailHook>,
}

impl Session {
    fn start() -> Option<Self> {
        let vm_id = transport::parse_vm_id(std::env::args_os())?;
        let port = std::env::var("ACCESSKIT_DVC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(transport::DEFAULT_PORT);
        let client: SharedClient = Arc::new(Mutex::new(ClientConnection::new("dvc_plugin")));
        let (actions_tx, actions_rx) = std::sync::mpsc::channel();
        let distro = rail::default_distro();
        if distro.is_none() {
            warn!("could not determine the default WSL distro; title suffixes won't be stripped");
        }
        let shared = Arc::new(RailShared {
            client: client.clone(),
            actions: actions_tx,
            distro,
            registry: Mutex::new(Registry::default()),
        });
        let hook = rail::start(shared.clone());
        let sink_client = client.clone();
        let sink_shared = shared.clone();
        let pump = transport::spawn_pump(vm_id, port, client, actions_rx, move |event| {
            handle_event(&sink_client, &sink_shared, event);
        });
        Some(Self { pump, hook: Some(hook) })
    }

    fn stop(mut self) {
        if let Some(hook) = self.hook.take() {
            hook.stop();
        }
        self.pump.stop();
    }
}

/// Pump-thread event sink: keep the registry current and route deltas to
/// bound RAIL windows.
fn handle_event(client: &SharedClient, shared: &Arc<RailShared>, event: ClientEvent) {
    match event {
        ClientEvent::Connected => info!("remote session established"),
        ClientEvent::WindowAdded { window } => {
            let locked = client.lock().unwrap();
            let info = locked.window_info(window);
            info!(
                "remote window added: {} title={:?} app={:?}",
                window.0,
                info.map(|i| i.title.as_str()),
                info.and_then(|i| i.app.app_id.as_deref())
            );
            let (title, app_id) = match info {
                Some(i) => (i.title.clone(), i.app.app_id.clone()),
                None => return,
            };
            drop(locked);
            shared.registry.lock().unwrap().window_added(window, title, app_id);
        }
        ClientEvent::WindowRemoved { window } => {
            info!("remote window removed: {}", window.0);
            let hwnd = shared.registry.lock().unwrap().window_removed(window);
            if let Some(hwnd) = hwnd {
                accesskit_remote_windows::post_detach(hwnd_from_key(hwnd));
            }
        }
        ClientEvent::TreeUpdated { window, update } => {
            debug!("tree updated: window {} ({} nodes)", window.0, update.nodes.len());
            let hwnd = shared.registry.lock().unwrap().bound_hwnd(window);
            if let Some(hwnd) = hwnd {
                accesskit_remote_windows::post_delta(hwnd_from_key(hwnd), update);
            }
        }
        ClientEvent::FocusChanged { window } => {
            debug!("remote focus: {:?}", window.map(|w| w.0))
        }
        ClientEvent::Pong { seq } => debug!("pong {seq}"),
        ClientEvent::Closed { reason } => info!("remote session closed: {reason}"),
    }
}

fn hwnd_from_key(key: isize) -> accesskit_remote_windows::HWND {
    accesskit_remote_windows::HWND(key as _)
}

#[implement(IWTSPlugin)]
pub struct AccessKitDvcPlugin {
    session: Mutex<Option<Session>>,
}

impl AccessKitDvcPlugin {
    pub fn new() -> Self {
        Self { session: Mutex::new(None) }
    }

    fn create_listener(
        &self,
        channel_mgr: &IWTSVirtualChannelManager,
        channel_name: &str,
    ) -> Result<IWTSListener> {
        debug!("Creating listener with name {channel_name}");
        let callback: IWTSListenerCallback =
            AccessKitListenerCallback::new(channel_name.to_owned()).into();
        // Bind the NUL-terminated name to a local so the pointer stays valid
        // for the duration of the CreateListener call.
        let name_c = format!("{channel_name}\0");
        unsafe { channel_mgr.CreateListener(PCSTR::from_raw(name_c.as_ptr()), 0, &callback) }
    }
}

impl IWTSPlugin_Impl for AccessKitDvcPlugin_Impl {
    fn Initialize(&self, pchannelmgr: Ref<'_, IWTSVirtualChannelManager>) -> Result<()> {
        debug!("Initialize");
        let channel_mgr = match pchannelmgr.as_ref() {
            Some(m) => m,
            None => {
                error!("No channel manager given on Initialize");
                return Err(Error::from(E_UNEXPECTED));
            }
        };
        self.create_listener(channel_mgr, CHANNEL_NAME)?;
        Ok(())
    }

    fn Connected(&self) -> Result<()> {
        info!("Client connected");
        let mut session = self.session.lock().unwrap();
        if session.is_some() {
            warn!("Connected with a session already running; keeping it");
            return Ok(());
        }
        match Session::start() {
            Some(s) => *session = Some(s),
            None => warn!("no /v:<vm-id> on the host command line; hvsocket client not started"),
        }
        Ok(())
    }

    fn Disconnected(&self, dwdisconnectcode: u32) -> Result<()> {
        info!("Client disconnected with {dwdisconnectcode}");
        if let Some(session) = self.session.lock().unwrap().take() {
            session.stop();
        }
        Ok(())
    }

    fn Terminated(&self) -> Result<()> {
        info!("Client terminated");
        if let Some(session) = self.session.lock().unwrap().take() {
            session.stop();
        }
        Ok(())
    }
}
