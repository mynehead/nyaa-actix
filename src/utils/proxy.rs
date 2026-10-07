//! Finding the visitor's address behind a reverse proxy (see `utils::client_ip`).
//!
//! Without a reverse proxy that is the address of the TCP connection. Behind one (nginx,
//! Caddy, Traefik, a Docker port mapping) the connection comes from the proxy, so every
//! visitor would share its address. `TRUSTED_PROXIES` lists the proxies' addresses or
//! networks; only for connections from those is `X-Forwarded-For` read, from the right,
//! skipping further trusted hops, so a visitor can't pick their own address by sending
//! the header themselves.

use std::net::{IpAddr, SocketAddr};

use actix_web::http::header::HeaderMap;

/// One `TRUSTED_PROXIES` entry: an address, or a network in CIDR notation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// `10.0.0.1`, `172.16.0.0/12`, `::1` or `fd00::/8`.
    pub fn parse(s: &str) -> Option<IpNet> {
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr.parse::<IpAddr>().ok()?, Some(prefix.parse::<u8>().ok()?)),
            None => (s.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(IpNet { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, normalize(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// Parses the comma separated `TRUSTED_PROXIES` list; `Err` names the first bad entry.
pub fn parse_trusted_proxies(value: &str) -> Result<Vec<IpNet>, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| IpNet::parse(s).ok_or_else(|| format!("TRUSTED_PROXIES: `{s}` is not an IP address or network")))
        .collect()
}

/// A dual-stack socket reports IPv4 clients as `::ffff:a.b.c.d`; treat those as IPv4.
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// The client's address: the connection's peer, or, when the peer is a trusted proxy, the
/// last address in `X-Forwarded-For` that isn't one.
pub fn resolve(peer: Option<SocketAddr>, headers: &HeaderMap, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = normalize(peer?.ip());
    let is_trusted = |ip: IpAddr| trusted.iter().any(|net| net.contains(ip));
    if !is_trusted(peer) {
        return Some(peer);
    }
    let mut client = peer;
    // Several headers are one list, in order; each proxy appends the address it saw
    let hops: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    for hop in hops.iter().rev() {
        // A garbled entry can't be trusted further; keep the last good address
        let Ok(ip) = hop.parse::<IpAddr>() else { break };
        client = normalize(ip);
        if !is_trusted(client) {
            break;
        }
    }
    Some(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::header::{HeaderName, HeaderValue};

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in xff {
            map.append(HeaderName::from_static("x-forwarded-for"), HeaderValue::from_str(v).unwrap());
        }
        map
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn peer(s: &str) -> Option<SocketAddr> {
        Some(SocketAddr::new(ip(s), 1234))
    }

    #[test]
    fn parses_addresses_and_networks() {
        let nets = parse_trusted_proxies(" 127.0.0.1, 172.16.0.0/12 ,fd00::/8,").unwrap();
        assert_eq!(nets.len(), 3);
        assert!(nets[0].contains(ip("127.0.0.1")) && !nets[0].contains(ip("127.0.0.2")));
        assert!(nets[1].contains(ip("172.31.255.1")) && !nets[1].contains(ip("172.32.0.1")));
        assert!(nets[2].contains(ip("fd12::1")) && !nets[2].contains(ip("fe80::1")));
        assert!(IpNet::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
        assert!(IpNet::parse("10.0.0.0/8").unwrap().contains(ip("::ffff:10.1.2.3")));
        for bad in ["10.0.0.0/33", "::/129", "nope", "10.0.0.0/", "/8"] {
            assert!(parse_trusted_proxies(bad).is_err(), "{bad}");
        }
        assert!(parse_trusted_proxies("").unwrap().is_empty());
    }

    #[test]
    fn forwarded_for_is_ignored_unless_the_peer_is_trusted() {
        let trusted = parse_trusted_proxies("172.16.0.0/12").unwrap();
        let spoofed = headers(&["6.6.6.6"]);
        assert_eq!(resolve(peer("203.0.113.9"), &spoofed, &trusted), Some(ip("203.0.113.9")));
        assert_eq!(resolve(peer("172.17.0.1"), &spoofed, &[]), Some(ip("172.17.0.1")));
    }

    #[test]
    fn takes_the_last_untrusted_hop() {
        let trusted = parse_trusted_proxies("172.16.0.0/12, 10.0.0.5").unwrap();
        // The client claims 6.6.6.6; the outer proxy saw 203.0.113.9, the inner one 10.0.0.5
        let xff = headers(&["6.6.6.6, 203.0.113.9", "10.0.0.5"]);
        assert_eq!(resolve(peer("172.17.0.1"), &xff, &trusted), Some(ip("203.0.113.9")));
        // No header: the proxy itself
        assert_eq!(resolve(peer("172.17.0.1"), &headers(&[]), &trusted), Some(ip("172.17.0.1")));
        // Garbage stops the walk at the last good address
        let xff = headers(&["203.0.113.9, junk, 10.0.0.5"]);
        assert_eq!(resolve(peer("172.17.0.1"), &xff, &trusted), Some(ip("10.0.0.5")));
        // IPv4-mapped peers match IPv4 networks
        let xff = headers(&["2001:db8::7"]);
        assert_eq!(resolve(peer("::ffff:172.17.0.1"), &xff, &trusted), Some(ip("2001:db8::7")));
        assert_eq!(resolve(None, &xff, &trusted), None);
    }
}
