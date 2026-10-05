pub mod pagination;
pub mod tera_filters;

use std::net::{IpAddr, SocketAddr};

use actix_web::HttpRequest;

/// The visitor's IP, packed like upstream (16 bytes). Behind a reverse proxy the TCP peer is
/// the proxy, so `behind_proxy` reads `Forwarded`/`X-Forwarded-For` instead; leave it off
/// otherwise, since anyone can send those headers.
pub fn client_ip(req: &HttpRequest, behind_proxy: bool) -> Option<Vec<u8>> {
    let ip = if behind_proxy {
        let info = req.connection_info();
        let addr = info.realip_remote_addr()?.trim_matches(['[', ']']).to_string();
        addr.parse::<IpAddr>().ok()
            .or_else(|| addr.parse::<SocketAddr>().ok().map(|a| a.ip()))
    } else {
        req.peer_addr().map(|a| a.ip())
    }?;
    Some(pack_ip(ip))
}

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
    fn sanitize_removes_control_characters() {
        assert_eq!(sanitize_string("a\u{0}b\nc\td é"), "abcd é");
    }
}
