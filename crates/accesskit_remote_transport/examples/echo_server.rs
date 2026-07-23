//! Manual cross-boundary test, guest side: `cargo run --example echo_server
//! [port]` inside WSL, then run `echo_client` on the Windows host.

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    use std::io::{Read, Write};

    let port: u32 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(52001);
    let listener = accesskit_remote_transport::vsock::listen(port)?;
    println!("listening on vsock port {port}");
    let (mut stream, peer) = listener.accept()?;
    println!("accepted from {peer:?}");
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    println!("received {n} bytes");
    stream.write_all(b"echo:")?;
    stream.write_all(&buf[..n])?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("echo_server runs on Linux only");
}
