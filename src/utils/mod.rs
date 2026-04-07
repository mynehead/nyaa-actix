pub mod pagination;

use std::net::IpAddr;

pub fn pack_ip(addr: IpAddr) -> Vec<u8> {
    match addr {
        IpAddr::V4(v4) => {
            let mut buf = vec![0u8; 16];
            let octets = v4.octets();
            buf[12..].copy_from_slice(&octets);
            buf
        }
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

pub fn sanitize_string(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
