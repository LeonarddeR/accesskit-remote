//! Manual cross-boundary test, host side: `cargo run --example echo_client
//! <vm-id> [port]` on Windows while `echo_server` runs inside WSL.

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use std::io::{Read, Write};

    let vm_id = std::env::args()
        .nth(1)
        .expect("usage: echo_client <vm-id> [port]")
        .parse()
        .expect("invalid VM ID");
    let port: u32 = std::env::args()
        .nth(2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(52001);
    let mut stream = accesskit_remote_transport::hvsocket::connect(vm_id, port)?;
    println!("connected to vsock port {port} in VM {vm_id}");
    stream.write_all(b"hello over hvsocket")?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    println!("reply: {}", String::from_utf8_lossy(&buf));
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("echo_client runs on Windows only");
}
