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
    /// Upstream ENABLE_GRAVATAR: avatars on profiles and comments. Waits on the comments feature.
    #[allow(dead_code)]
    pub enable_gravatar: bool,
    /// Upstream MAINTENANCE_MODE: turns off uploads, registration and login with a notice. Not enforced yet.
    #[allow(dead_code)]
    pub maintenance_mode: bool,
    /// Public base URL of the site, used in the .torrent comment field.
    pub site_url: String,
    /// Announce URLs written into magnets and .torrent files, own tracker first.
    pub tracker_urls: Vec<String>,
    /// Take the visitor's IP from `Forwarded`/`X-Forwarded-For` (only behind a reverse proxy).
    pub behind_reverse_proxy: bool,
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
            results_per_page: env::var("RESULTS_PER_PAGE")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(75),
            max_pages: env::var("MAX_PAGES")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            torrent_storage_path: env::var("TORRENT_STORAGE_PATH")
                .unwrap_or_else(|_| "./torrents".into()),
            enable_gravatar: env::var("ENABLE_GRAVATAR")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(false),
            maintenance_mode: env::var("MAINTENANCE_MODE")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(false),
            site_url: env::var("SITE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_string(),
            tracker_urls: ["TRACKER_ANNOUNCE_URLS", "TRACKER_EXTRA_URLS"]
                .iter()
                .flat_map(|key| split_list(&env::var(key).unwrap_or_default()))
                .collect(),
            behind_reverse_proxy: env::var("BEHIND_REVERSE_PROXY")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(false),
        }
    }

    pub fn trackers(&self) -> Vec<&str> {
        self.tracker_urls.iter().map(String::as_str).collect()
    }
}

/// Splits a comma separated list, dropping blanks.
fn split_list(value: &str) -> Vec<String> {
    value.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_list_trims_and_drops_blanks() {
        assert_eq!(split_list(" udp://a/announce , ,http://b/announce,"), vec!["udp://a/announce", "http://b/announce"]);
        assert!(split_list("").is_empty());
    }
}
