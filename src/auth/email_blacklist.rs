//! Upstream's registration email blacklist: EMAIL_BLACKLIST rejects addresses matching a
//! pattern (by default every Microsoft mail domain, since Hotmail drops mail from "untrusted"
//! senders, so those users could never get their verification mail), and
//! EMAIL_SERVER_BLACKLIST rejects domains whose mail servers resolve to a listed IP.

use std::env;
use std::net::IpAddr;

use hickory_resolver::proto::rr::RData;
use hickory_resolver::TokioResolver;
use regex::{Regex, RegexBuilder};

/// Upstream's wording for both checks.
pub const BLACKLISTED: &str = "Blacklisted email provider";

/// Upstream's default EMAIL_BLACKLIST: (hopefully) every Microsoft email domain.
const DEFAULT_PATTERNS: &[&str] = &[
    r"@hotmail\.(co|co\.uk|com|de|dk|eu|fr|it|net|org|se)",
    r"@live\.(co|co.uk|com|de|dk|eu|fr|it|net|org|se|no)",
    r"@outlook\.(at|be|cl|co|co\.(id|il|nz|th)|com|com\.(ar|au|au|br|gr|pe|tr|vn)|cz|de|de|dk|dk|es|eu|fr|fr|hu|ie|in|it|it|jp|kr|lv|my|org|ph|pt|sa|se|sg|sk)",
    r"@(msn\.com|passport\.(com|net))",
];

#[derive(Clone, Debug, Default)]
pub struct EmailBlacklist {
    /// EMAIL_BLACKLIST: case-insensitive patterns searched anywhere in the address.
    pub patterns: Vec<Regex>,
    /// EMAIL_SERVER_BLACKLIST: mail server addresses (the A/AAAA records of the domain's MX hosts).
    pub servers: Vec<IpAddr>,
}

impl EmailBlacklist {
    /// EMAIL_BLACKLIST is a whitespace separated list of regexes (unset: upstream's Microsoft
    /// list, empty: off); EMAIL_SERVER_BLACKLIST a comma separated list of IPs (default: off).
    pub fn from_env() -> Self {
        let patterns = match env::var("EMAIL_BLACKLIST") {
            Ok(value) => value.split_whitespace().map(String::from).collect(),
            Err(_) => DEFAULT_PATTERNS.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        };
        let servers = env::var("EMAIL_SERVER_BLACKLIST").unwrap_or_default();
        EmailBlacklist {
            patterns: patterns
                .iter()
                .map(|p| compile(p).unwrap_or_else(|e| panic!("EMAIL_BLACKLIST: bad pattern {p:?}: {e}")))
                .collect(),
            servers: servers
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap_or_else(|_| panic!("EMAIL_SERVER_BLACKLIST: {s:?} is not an IP address")))
                .collect(),
        }
    }

    /// Upstream's defaults, for tests.
    #[cfg(test)]
    pub fn upstream_defaults() -> Self {
        EmailBlacklist { patterns: DEFAULT_PATTERNS.iter().map(|p| compile(p).unwrap()).collect(), servers: vec![] }
    }

    /// Whether `email` matches an EMAIL_BLACKLIST pattern.
    pub fn blocks_address(&self, email: &str) -> bool {
        self.patterns.iter().any(|p| p.is_match(email))
    }

    /// Whether the domain's mail servers include an EMAIL_SERVER_BLACKLIST address. Like
    /// upstream, a failed DNS lookup lets the address through.
    pub async fn blocks_server(&self, email: &str) -> bool {
        if self.servers.is_empty() {
            return false;
        }
        let domain = email.rsplit_once('@').map_or(email, |(_, d)| d);
        let resolver = match TokioResolver::builder_tokio().and_then(|b| b.build()) {
            Ok(resolver) => resolver,
            Err(e) => {
                log::error!("Unable to set up DNS for the email server blacklist: {e} - ignoring");
                return false;
            }
        };
        let mx = match resolver.mx_lookup(domain).await {
            Ok(mx) => mx,
            Err(e) => {
                log::error!("Unable to query MX records for email: {email} ({e}) - ignoring");
                return false;
            }
        };
        for record in mx.answers() {
            let RData::MX(mx) = &record.data else { continue };
            match resolver.lookup_ip(mx.exchange.clone()).await {
                Ok(ips) => {
                    if let Some(ip) = ips.iter().find(|ip| self.servers.contains(ip)) {
                        log::warn!("Rejected email {email} due to blacklisted mailserver ({ip}, {})", mx.exchange);
                        return true;
                    }
                }
                Err(e) => {
                    log::warn!("Failed to query A records for mailserver: {} ({email}, {e}) - ignoring", mx.exchange)
                }
            }
        }
        false
    }
}

fn compile(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern).case_insensitive(true).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_list_blocks_microsoft_domains_only() {
        let list = EmailBlacklist::upstream_defaults();
        for email in
            ["a@hotmail.com", "a@HOTMAIL.CO.UK", "a@live.de", "a@outlook.com.au", "a@msn.com", "a@passport.net"]
        {
            assert!(list.blocks_address(email), "{email}");
        }
        for email in ["a@gmail.com", "hotmail@example.com", "a@outlook.example"] {
            assert!(!list.blocks_address(email), "{email}");
        }
    }

    #[test]
    fn empty_list_blocks_nothing() {
        assert!(!EmailBlacklist::default().blocks_address("a@hotmail.com"));
    }

    /// The commented-out EMAIL_BLACKLIST in .env.example, once uncommented, is exactly the default.
    #[test]
    fn env_example_spells_out_the_default() {
        let example = std::fs::read_to_string(".env.example").unwrap();
        let line = example.lines().find_map(|l| l.strip_prefix("# EMAIL_BLACKLIST='")).unwrap();
        let line = format!("EMAIL_BLACKLIST='{line}");
        let (_, value) = dotenvy::from_read_iter(line.as_bytes()).next().unwrap().unwrap();
        assert_eq!(value.split_whitespace().collect::<Vec<_>>(), DEFAULT_PATTERNS);
    }

    #[actix_web::test]
    async fn no_server_list_skips_dns() {
        assert!(!EmailBlacklist::default().blocks_server("a@example.com").await);
    }
}
