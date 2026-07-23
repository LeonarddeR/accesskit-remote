//! `AF_VSOCK` listener for the Linux guest side.
//!
//! Inside a WSL2 distro, a vsock port is reachable from the Windows host
//! via `AF_HYPERV` with the service ID from
//! [`service_id_for_port`](crate::hvsocket_addr::service_id_for_port);
//! no host-side registry configuration is required.

use socket2::{Domain, SockAddr, Socket, Type};
use std::io;

pub const VMADDR_CID_ANY: u32 = u32::MAX;

pub fn listen(port: u32) -> io::Result<Socket> {
    let socket = Socket::new(Domain::VSOCK, Type::STREAM, None)?;
    socket.bind(&SockAddr::vsock(VMADDR_CID_ANY, port))?;
    socket.listen(4)?;
    Ok(socket)
}
