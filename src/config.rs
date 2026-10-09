use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub secret_key: String,
    pub site_name: String,
    pub site_flavor: String,
    pub results_per_page: i64,
    /// Upstream MAX_PAGES: cap on how deep listings can be paged (0 = no cap). Not enforced yet.
    #[allow(dead_code)]
    pub max_pages: i64,
    pub torrent_storage_path: String,
    /// Where uploaded avatars are kept, as `{user_id}.png`.
    pub avatar_storage_path: String,
    /// Upstream ENABLE_GRAVATAR: Gravatar for users without an uploaded avatar.
    pub enable_gravatar: bool,
    /// Upstream MAINTENANCE_MODE: turns off uploads, registration and login with a notice. Not enforced yet.
    #[allow(dead_code)]
    pub maintenance_mode: bool,
    /// Public base URL of the site, used in the .torrent comment field.
    pub site_url: String,
    /// Announce URLs written into magnets and .torrent files, own tracker first.
    pub tracker_urls: Vec<String>,
    /// TRUSTED_PROXIES: reverse proxies whose `X-Forwarded-For` names the visitor.
    pub trusted_proxies: Vec<crate::utils::proxy::IpNet>,
    /// Upstream RATELIMIT_ACCOUNT_AGE, in seconds: accounts must be older than this to report torrents.
    pub ratelimit_account_age: i64,
    /// Upstream EDITING_TIME_LIMIT, in seconds: how long after posting a comment its author may
    /// still edit or delete it (0 = no limit).
    pub editing_time_limit: i64,
    /// Meilisearch for text search and stats sorts (MEILI_URL and friends); None keeps search on SQLite.
    pub meili: Option<crate::search::meili::Meili>,
    /// The tracker's management API (TRACKER_API_URL and TRACKER_API_KEY) for the whitelist
    /// and stats; None runs the site without talking to a tracker.
    pub tracker: Option<crate::tracker::Tracker>,
    /// Who may apply for trusted status (upstream's "Trusted Requirements").
    pub trusted: TrustedConfig,
    /// How many support tickets and replies a user may send.
    pub tickets: TicketConfig,
}

/// Rate limits for support tickets. Staff (HandleTickets) are not limited. 0 turns a limit off.
#[derive(Clone, Debug)]
pub struct TicketConfig {
    /// TICKET_RATE_LIMIT: new tickets per user per TICKET_RATE_WINDOW.
    pub max_tickets: i64,
    /// TICKET_RATE_WINDOW, in seconds.
    pub ticket_window_secs: i64,
    /// TICKET_REPLY_RATE_LIMIT: replies per user per TICKET_REPLY_RATE_WINDOW.
    pub max_replies: i64,
    /// TICKET_REPLY_RATE_WINDOW, in seconds.
    pub reply_window_secs: i64,
}

impl Default for TicketConfig {
    fn default() -> Self {
        TicketConfig { max_tickets: 3, ticket_window_secs: 24 * 3600, max_replies: 20, reply_window_secs: 3600 }
    }
}

impl TicketConfig {
    fn from_env() -> Self {
        let num = |key: &str, default: i64| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let d = TicketConfig::default();
        TicketConfig {
            max_tickets: num("TICKET_RATE_LIMIT", d.max_tickets),
            ticket_window_secs: num("TICKET_RATE_WINDOW", d.ticket_window_secs),
            max_replies: num("TICKET_REPLY_RATE_LIMIT", d.max_replies),
            reply_window_secs: num("TICKET_REPLY_RATE_WINDOW", d.reply_window_secs),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrustedConfig {
    /// TRUSTED_MIN_UPLOADS: non-remake uploads needed to apply.
    pub min_uploads: i64,
    /// TRUSTED_MIN_DOWNLOADS: total downloads of those uploads needed to apply.
    pub min_downloads: i64,
    /// TRUSTED_REAPPLY_COOLDOWN: days after a rejection before applying again.
    pub reapply_cooldown_days: i64,
}

impl Default for TrustedConfig {
    fn default() -> Self {
        TrustedConfig { min_uploads: 10, min_downloads: 10000, reapply_cooldown_days: 90 }
    }
}

impl TrustedConfig {
    fn from_env() -> Self {
        let num = |key: &str, default: i64| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let d = TrustedConfig::default();
        TrustedConfig {
            min_uploads: num("TRUSTED_MIN_UPLOADS", d.min_uploads),
            min_downloads: num("TRUSTED_MIN_DOWNLOADS", d.min_downloads),
            reapply_cooldown_days: num("TRUSTED_REAPPLY_COOLDOWN", d.reapply_cooldown_days),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();
        let secret_key = env::var("SECRET_KEY").expect("SECRET_KEY must be set");
        // actix_web::cookie::Key::from panics on anything shorter than 64 bytes
        assert!(
            secret_key.len() >= 64,
            "SECRET_KEY must be at least 64 bytes (got {}); generate one with `openssl rand -hex 64`",
            secret_key.len()
        );
        Config {
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "nyaa.db".into()),
            secret_key,
            site_name: env::var("SITE_NAME").unwrap_or_else(|_| "Nyaa".into()),
            site_flavor: env::var("SITE_FLAVOR").unwrap_or_else(|_| "nyaa".into()),
            results_per_page: env::var("RESULTS_PER_PAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(75),
            max_pages: env::var("MAX_PAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            torrent_storage_path: env::var("TORRENT_STORAGE_PATH").unwrap_or_else(|_| "./torrents".into()),
            avatar_storage_path: env::var("AVATAR_STORAGE_PATH").unwrap_or_else(|_| "./avatars".into()),
            enable_gravatar: env::var("ENABLE_GRAVATAR").ok().and_then(|v| v.parse().ok()).unwrap_or(false),
            maintenance_mode: env::var("MAINTENANCE_MODE").ok().and_then(|v| v.parse().ok()).unwrap_or(false),
            site_url: env::var("SITE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_string(),
            tracker_urls: ["TRACKER_ANNOUNCE_URLS", "TRACKER_EXTRA_URLS"]
                .iter()
                .flat_map(|key| split_list(&env::var(key).unwrap_or_default()))
                .collect(),
            trusted_proxies: crate::utils::proxy::parse_trusted_proxies(
                &env::var("TRUSTED_PROXIES").unwrap_or_default(),
            )
            .unwrap_or_else(|e| panic!("{e}")),
            ratelimit_account_age: env::var("RATELIMIT_ACCOUNT_AGE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(7 * 24 * 3600),
            editing_time_limit: env::var("EDITING_TIME_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            meili: crate::search::meili::Meili::from_env(),
            tracker: crate::tracker::Tracker::from_env(),
            trusted: TrustedConfig::from_env(),
            tickets: TicketConfig::from_env(),
        }
    }

    /// Defaults for handler tests: no files, no trackers, no Meilisearch.
    #[cfg(test)]
    pub fn for_tests() -> Config {
        Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: String::new(),
            enable_gravatar: false,
            maintenance_mode: false,
            site_url: String::new(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            trusted: Default::default(),
            tickets: Default::default(),
        }
    }

    pub fn trackers(&self) -> Vec<&str> {
        self.tracker_urls.iter().map(String::as_str).collect()
    }
}

/// Splits a comma separated list, dropping blanks.
fn split_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_list_trims_and_drops_blanks() {
        assert_eq!(
            split_list(" udp://a/announce , ,http://b/announce,"),
            vec!["udp://a/announce", "http://b/announce"]
        );
        assert!(split_list("").is_empty());
    }
}
