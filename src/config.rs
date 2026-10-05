use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub secret_key: String,
    pub site_name: String,
    pub site_flavor: String,
    pub results_per_page: i64,
    pub max_pages: i64,
    pub torrent_storage_path: String,
    pub enable_gravatar: bool,
    pub maintenance_mode: bool,
}

impl Config {
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();
        let secret_key = env::var("SECRET_KEY").expect("SECRET_KEY must be set");
        // actix's cookie Key::from panics on anything shorter than 64 bytes.
        assert!(secret_key.len() >= 64, "SECRET_KEY must be at least 64 bytes long");
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
        }
    }
}
