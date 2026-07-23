//! Hyper-V socket addressing shared by both sides of the WSL boundary.

use uuid::Uuid;

/// Maps a Linux `AF_VSOCK` port to the Hyper-V service ID the Windows host
/// must connect to, per the Linux hvsocket GUID template
/// `<port>-facb-11e6-bd58-64006a7986d3`.
pub fn service_id_for_port(port: u32) -> Uuid {
    Uuid::from_fields(
        port,
        0xfacb,
        0x11e6,
        &[0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_wslg_rdp_service_id() {
        assert_eq!(
            service_id_for_port(1).to_string(),
            "00000001-facb-11e6-bd58-64006a7986d3"
        );
        assert_eq!(
            service_id_for_port(52000).to_string(),
            "0000cb20-facb-11e6-bd58-64006a7986d3"
        );
    }
}
