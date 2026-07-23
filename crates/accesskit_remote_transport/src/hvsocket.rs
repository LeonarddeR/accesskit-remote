//! `AF_HYPERV` connector for the Windows host side.

use crate::hvsocket_addr::service_id_for_port;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use uuid::Uuid;

pub const AF_HYPERV: i32 = 34;
pub const HV_PROTOCOL_RAW: i32 = 1;

const SOCKADDR_HV_LEN: usize = 36;

/// Connects to an `AF_VSOCK` listener on `port` inside the utility VM
/// identified by `vm_id` (for WSL, the GUID from msrdc's `/v:` argument;
/// it changes on every VM boot).
pub fn connect(vm_id: Uuid, port: u32) -> io::Result<Socket> {
    let socket = Socket::new(
        Domain::from(AF_HYPERV),
        Type::STREAM,
        Some(Protocol::from(HV_PROTOCOL_RAW)),
    )?;
    socket.connect(&sockaddr_hv(vm_id, service_id_for_port(port)))?;
    Ok(socket)
}

/// Builds a `SOCKADDR_HV`: family and reserved u16s, then the VM and
/// service GUIDs in Windows binary (mixed-endian) layout.
fn sockaddr_hv(vm_id: Uuid, service_id: Uuid) -> SockAddr {
    let mut bytes = [0u8; SOCKADDR_HV_LEN];
    bytes[0..2].copy_from_slice(&(AF_HYPERV as u16).to_le_bytes());
    bytes[4..20].copy_from_slice(&vm_id.to_bytes_le());
    bytes[20..36].copy_from_slice(&service_id.to_bytes_le());
    let ((), addr) = unsafe {
        SockAddr::try_init(|storage, len| {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                storage.cast::<u8>(),
                SOCKADDR_HV_LEN,
            );
            *len = SOCKADDR_HV_LEN as _;
            Ok(())
        })
    }
    .expect("initializing a sockaddr from a fixed buffer cannot fail");
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_layout() {
        let vm_id = Uuid::parse_str("B318F509-F122-4F95-8E5E-2B741891823B").unwrap();
        let addr = sockaddr_hv(vm_id, service_id_for_port(1));
        assert_eq!(addr.len() as usize, SOCKADDR_HV_LEN);
        let raw = unsafe {
            std::slice::from_raw_parts(addr.as_ptr().cast::<u8>(), SOCKADDR_HV_LEN)
        };
        assert_eq!(&raw[0..2], &(AF_HYPERV as u16).to_le_bytes());
        assert_eq!(&raw[2..4], &[0, 0]);
        assert_eq!(&raw[4..20], &vm_id.to_bytes_le());
        assert_eq!(
            &raw[20..24],
            &[0x01, 0x00, 0x00, 0x00],
            "service GUID Data1 is little-endian port"
        );
    }
}
