//! Interactive smoke test against a running provider (e.g.
//! `accesskit_remoted`): connects, prints the announced windows and tree,
//! clicks the demo button, prints the resulting update, and exits.
//!
//! Usage:
//!   probe --tcp [PORT]
//!   probe --hvsocket <vm-id> [PORT]   (Windows only)

use accesskit_remote_client::{ClientConnection, ClientEvent};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stream = connect(&args)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;

    let mut client = ClientConnection::new("probe");
    let mut clicked = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 16384];

    while Instant::now() < deadline && !client.is_closed() {
        let out = client.take_output();
        if !out.is_empty() {
            stream.write_all(&out)?;
        }
        let chunk = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => &buf[..n],
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        for event in client.handle_input(chunk).map_err(std::io::Error::other)? {
            match event {
                ClientEvent::Connected => println!("connected and established"),
                ClientEvent::WindowAdded { window } => {
                    let info = client.window_info(window).unwrap().clone();
                    println!(
                        "window {}: '{}' app={} ({})",
                        window.0,
                        info.title,
                        info.app.name,
                        info.app.app_id.as_deref().unwrap_or("-")
                    );
                }
                ClientEvent::TreeUpdated { window, update } => {
                    println!("tree update for window {} ({} nodes):", window.0, update.nodes.len());
                    for (id, node) in &update.nodes {
                        println!(
                            "  node {}: {:?} '{}'",
                            id.0,
                            node.role(),
                            node.label().unwrap_or_default()
                        );
                    }
                    if !clicked {
                        clicked = true;
                        println!("clicking node 2...");
                        client
                            .request_action(
                                window,
                                accesskit::ActionRequest {
                                    action: accesskit::Action::Click,
                                    target_tree: update.tree_id,
                                    target_node: accesskit::NodeId(2),
                                    data: None,
                                },
                            )
                            .map_err(std::io::Error::other)?;
                    } else {
                        println!("live update received after click; done");
                        return Ok(());
                    }
                }
                ClientEvent::FocusChanged { window } => {
                    println!("focused window: {:?}", window.map(|w| w.0));
                }
                ClientEvent::WindowRemoved { window } => {
                    println!("window {} removed", window.0);
                }
                ClientEvent::Pong { seq } => println!("pong {seq}"),
                ClientEvent::Closed { reason } => println!("closed: {reason}"),
            }
        }
    }
    Ok(())
}

fn connect(args: &[String]) -> std::io::Result<accesskit_remote_transport::Socket> {
    match args.first().map(String::as_str) {
        Some("--tcp") | None => {
            let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(4750);
            accesskit_remote_transport::tcp::connect_local(port).map(Into::into)
        }
        #[cfg(windows)]
        Some("--hvsocket") => {
            let vm_id = args
                .get(1)
                .expect("usage: probe --hvsocket <vm-id> [PORT]")
                .parse::<uuid::Uuid>()
                .expect("invalid VM ID");
            let port: u32 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(4750);
            accesskit_remote_transport::hvsocket::connect(vm_id, port)
        }
        Some(other) => Err(std::io::Error::other(format!("unknown mode: {other}"))),
    }
}
