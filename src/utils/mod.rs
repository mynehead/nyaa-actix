pub mod pagination;
pub mod context;
pub mod tera_filters;
pub mod flash;
pub mod avatar;

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

/// Text form of an address stored by `pack_ip` (IPv4 in the last four bytes of 16, or 4 bytes).
pub fn ip_string(packed: &[u8]) -> Option<String> {
    let v4 = |b: &[u8]| std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string();
    match packed.len() {
        4 => Some(v4(packed)),
        16 if packed[..12].iter().all(|&b| b == 0) => Some(v4(&packed[12..])),
        16 => <[u8; 16]>::try_from(packed).ok().map(|b| std::net::Ipv6Addr::from(b).to_string()),
        _ => None,
    }
}

pub fn sanitize_string(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Like `sanitize_string`, but keeps line breaks and tabs, for Markdown fields
/// such as descriptions. Line endings are normalized to `\n`.
pub fn sanitize_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
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
    }

    #[test]
    fn ip_string_reverses_pack_ip() {
        for ip in ["192.168.1.2", "2001:db8::1"] {
            assert_eq!(ip_string(&pack_ip(ip.parse().unwrap())).as_deref(), Some(ip));
        }
        assert_eq!(ip_string(&[127, 0, 0, 1]).as_deref(), Some("127.0.0.1"));
        assert_eq!(ip_string(&[1, 2]), None);
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
