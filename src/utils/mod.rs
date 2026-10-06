pub mod avatar;
pub mod context;
pub mod flash;
pub mod pagination;
pub mod tera_filters;
pub mod throttle;

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

/// Reverses `pack_ip`; also reads 4-byte IPv4 values. `None` for any other length.
pub fn unpack_ip(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 if bytes[..12].iter().all(|&b| b == 0) && bytes[12..] != [0, 0, 0, 0] && bytes[12..] != [0, 0, 0, 1] => {
            Some(IpAddr::from(<[u8; 4]>::try_from(&bytes[12..]).ok()?))
        }
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => None,
    }
}

pub fn sanitize_string(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Like `sanitize_string`, but keeps line breaks and tabs, for Markdown fields
/// such as descriptions. Line endings are normalized to `\n`.
pub fn sanitize_text(s: &str) -> String {
    s.replace("\r\n", "\n").chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_ipv4_into_last_four_bytes() {
        let packed = pack_ip("192.168.1.2".parse().unwrap());
        assert_eq!(packed.len(), 16);
        assert_eq!(&packed[..12], &[0u8; 12]);
        assert_eq!(&packed[12..], &[192, 168, 1, 2]);
    }

    #[test]
    fn packs_ipv6_as_is() {
        let addr: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert_eq!(pack_ip(IpAddr::V6(addr)), addr.octets().to_vec());
        for ip in ["192.168.1.2", "2001:db8::1", "::1", "::"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert_eq!(unpack_ip(&pack_ip(ip)), Some(ip));
        }
        assert_eq!(unpack_ip(&[10, 0, 0, 1]), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(unpack_ip(&[1, 2]), None);
    }

    #[test]
    fn sanitize_text_keeps_line_breaks() {
        assert_eq!(sanitize_text("**a**\r\n- b\n\tc\u{0}"), "**a**\n- b\n\tc");
    }

    #[test]
    fn sanitize_removes_control_characters() {
        assert_eq!(sanitize_string("a\u{0}b\nc\td é"), "abcd é");
    }
}
