use std::env;

/// Upstream's Gravatar endpoint, used unless GRAVATAR_URL points elsewhere.
pub const DEFAULT_GRAVATAR_URL: &str = "https://www.gravatar.com/avatar";

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub secret_key: String,
    pub site_name: String,
    /// Upstream GLOBAL_SITE_NAME: what both sites are called together, used in mail subjects.
    pub global_site_name: String,
    pub site_flavor: String,
    /// Upstream EXTERNAL_URLS: the sister site's address for the "Fap" (on nyaa) or "Fun"
    /// (on sukebei) navbar link; None hides the link.
    pub sister_site_url: Option<String>,
    pub results_per_page: i64,
    /// Upstream MAX_PAGES: cap on how deep torrent listings can be paged (0 = no cap).
    pub max_pages: i64,
    pub torrent_storage_path: String,
    /// Where uploaded avatars are kept, as `{user_id}.png`.
    pub avatar_storage_path: String,
    /// Upstream ENABLE_GRAVATAR: Gravatar for users without an uploaded avatar.
    pub enable_gravatar: bool,
    /// Upstream ENABLE_SHOW_STATS: seeders, leechers and completed on the torrent page;
    /// off shows "Coming soon" instead.
    pub show_stats: bool,
    /// Upstream MAX_FILES_VIEW: torrents with more files show "Too many files to display."
    pub max_files_view: usize,
    /// Upstream ENFORCE_MAIN_ANNOUNCE_URL with MAIN_ANNOUNCE_URL: uploads must list this
    /// tracker. None accepts any trackers.
    pub required_announce_url: Option<String>,
    /// GRAVATAR_URL: base of a Gravatar-compatible avatar service (gravatar.com, Libravatar
    /// or a self-hosted instance), without the trailing slash; the hash is appended to it.
    pub gravatar_url: String,
    /// GRAVATAR_HASH=sha256: hash the email with SHA-256 instead of upstream's MD5.
    pub gravatar_sha256: bool,
    /// Upstream MAINTENANCE_MODE and friends: a read-only site with a notice.
    pub maintenance: MaintenanceConfig,
    /// Upstream RAID_MODE_LIMIT_REGISTER and RAID_MODE_REGISTER_MESSAGE.
    pub raid_mode: RaidModeConfig,
    /// REGISTRATION_MODE and INVITE_EXPIRY_DAYS: open sign-up, invite codes only, or closed.
    pub registration: RegistrationConfig,
    /// Public base URL of the site, used in the .torrent comment field.
    pub site_url: String,
    /// Announce URLs written into magnets and .torrent files, own tracker first.
    pub tracker_urls: Vec<String>,
    /// TRUSTED_PROXIES: reverse proxies whose `X-Forwarded-For` names the visitor.
    pub trusted_proxies: Vec<crate::utils::proxy::IpNet>,
    /// Upstream RATELIMIT_ACCOUNT_AGE, in seconds: accounts must be older than this to report torrents.
    pub ratelimit_account_age: i64,
    /// Upstream EDITING_TIME_LIMIT, in seconds: how long after posting a comment its author may
    /// still edit or delete it (0 = no limit). Moderators and admins have no limit.
    pub editing_time_limit: i64,
    /// Upstream's upload rate limit for accounts younger than RATELIMIT_ACCOUNT_AGE.
    pub upload_limit: UploadLimitConfig,
    /// Account mails: MAIL_BACKEND, USE_EMAIL_VERIFICATION, ALLOW_PASSWORD_RESET and friends.
    pub mail: crate::mail::MailConfig,
    /// Meilisearch for text search and stats sorts (MEILI_URL and friends); None keeps search on SQLite.
    pub meili: Option<crate::search::meili::Meili>,
    /// Upstream COUNT_CACHE_SIZE and COUNT_CACHE_DURATION: listing totals reused for a few
    /// seconds; None counts every time.
    pub count_cache: Option<std::sync::Arc<crate::search::count_cache::CountCache>>,
    /// The tracker's management API (TRACKER_API_URL and TRACKER_API_KEY) for the whitelist
    /// and stats; None runs the site without talking to a tracker.
    pub tracker: Option<crate::tracker::Tracker>,
    /// Who may apply for trusted status (upstream's "Trusted Requirements").
    pub trusted: TrustedConfig,
    /// How many support tickets and replies a user may send.
    pub tickets: TicketConfig,
    /// Two-factor sign-in (MFA_REQUIRED_LEVEL, MFA_ISSUER_NAME).
    pub mfa: crate::auth::mfa::MfaConfig,
    /// The ALTCHA captcha on login and registration (USE_CAPTCHA); None shows none.
    pub captcha: Option<crate::captcha::Captcha>,
    /// Upstream EMAIL_BLACKLIST and EMAIL_SERVER_BLACKLIST: email providers registration turns away.
    pub email_blacklist: crate::auth::email_blacklist::EmailBlacklist,
}

/// Upstream's maintenance mode: every page still shows, with `message` on top, but nothing
/// can be changed: form posts are turned back with the message and the API answers 503.
/// Logging in (and out) still works while `logins` is on.
#[derive(Clone, Debug)]
pub struct MaintenanceConfig {
    /// MAINTENANCE_MODE
    pub enabled: bool,
    /// MAINTENANCE_MODE_MESSAGE
    pub message: String,
    /// MAINTENANCE_MODE_LOGINS
    pub logins: bool,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        MaintenanceConfig {
            enabled: false,
            message: "Site is currently in read-only maintenance mode.".into(),
            logins: true,
        }
    }
}

impl MaintenanceConfig {
    fn from_env() -> Self {
        let flag = |key: &str, default: bool| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let d = MaintenanceConfig::default();
        MaintenanceConfig {
            enabled: flag("MAINTENANCE_MODE", d.enabled),
            message: env::var("MAINTENANCE_MODE_MESSAGE").ok().filter(|m| !m.trim().is_empty()).unwrap_or(d.message),
            logins: flag("MAINTENANCE_MODE_LOGINS", d.logins),
        }
    }
}

/// Who may create an account at /register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationMode {
    /// Anyone (upstream's behaviour).
    #[default]
    Open,
    /// Only with an invite code from /admin/invites.
    Invite,
    /// Nobody; accounts come from `create-user` or an admin.
    Closed,
}

#[derive(Clone, Debug)]
pub struct RegistrationConfig {
    /// REGISTRATION_MODE: open, invite or closed.
    pub mode: RegistrationMode,
    /// INVITE_EXPIRY_DAYS: how long a new invite code stays valid.
    pub invite_expiry_days: i64,
    /// INVITES_FOR_TRUSTED: invites every Trusted user gets once (0 = none; moderators can
    /// still give individual users invites on their page).
    pub invites_for_trusted: i64,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        RegistrationConfig { mode: RegistrationMode::Open, invite_expiry_days: 7, invites_for_trusted: 2 }
    }
}

impl RegistrationConfig {
    fn from_env() -> Self {
        let d = RegistrationConfig::default();
        let mode = match env::var("REGISTRATION_MODE").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
            "" | "open" => RegistrationMode::Open,
            "invite" => RegistrationMode::Invite,
            "closed" => RegistrationMode::Closed,
            other => panic!("REGISTRATION_MODE must be open, invite or closed, got {:?}", other),
        };
        RegistrationConfig {
            mode,
            invite_expiry_days: env::var("INVITE_EXPIRY_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&d: &i64| d > 0)
                .unwrap_or(d.invite_expiry_days),
            invites_for_trusted: env::var("INVITES_FOR_TRUSTED")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&n: &i64| n >= 0)
                .unwrap_or(d.invites_for_trusted),
        }
    }
}

/// Upstream's raid mode for registration: sign-ups still create an account, but it stays
/// inactive (no login, no verification mail) until a moderator activates it on the user's
/// page. Upstream's RAID_MODE_LIMIT_UPLOADS is not here because uploads need an account.
#[derive(Clone, Debug)]
pub struct RaidModeConfig {
    /// RAID_MODE_LIMIT_REGISTER
    pub limit_register: bool,
    /// RAID_MODE_REGISTER_MESSAGE, shown before the "ask a moderator" note.
    pub register_message: String,
}

impl Default for RaidModeConfig {
    fn default() -> Self {
        RaidModeConfig { limit_register: false, register_message: "Registration is currently being limited.".into() }
    }
}

impl RaidModeConfig {
    fn from_env() -> Self {
        let d = RaidModeConfig::default();
        RaidModeConfig {
            limit_register: env::var("RAID_MODE_LIMIT_REGISTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.limit_register),
            register_message: env::var("RAID_MODE_REGISTER_MESSAGE")
                .ok()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or(d.register_message),
        }
    }
}

/// Upstream's upload rate limit: accounts younger than RATELIMIT_ACCOUNT_AGE that may not
/// skip it ([`crate::auth::Permission::SkipUploadLimit`], Trusted and up) can upload
/// `max_burst` torrents within `burst_secs`; after that, each upload must wait `timeout_secs`
/// after the previous one. Uploads from the same IP address count too.
#[derive(Clone, Debug)]
pub struct UploadLimitConfig {
    /// RATELIMIT_UPLOADS: false turns the limit off.
    pub enabled: bool,
    /// MAX_UPLOAD_BURST
    pub max_burst: i64,
    /// UPLOAD_BURST_DURATION, in seconds.
    pub burst_secs: i64,
    /// UPLOAD_TIMEOUT, in seconds.
    pub timeout_secs: i64,
}

impl Default for UploadLimitConfig {
    fn default() -> Self {
        UploadLimitConfig { enabled: true, max_burst: 5, burst_secs: 45 * 60, timeout_secs: 15 * 60 }
    }
}

impl UploadLimitConfig {
    fn from_env() -> Self {
        let num = |key: &str, default: i64| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let d = UploadLimitConfig::default();
        UploadLimitConfig {
            enabled: env::var("RATELIMIT_UPLOADS").ok().and_then(|v| v.parse().ok()).unwrap_or(d.enabled),
            max_burst: num("MAX_UPLOAD_BURST", d.max_burst),
            burst_secs: num("UPLOAD_BURST_DURATION", d.burst_secs),
            timeout_secs: num("UPLOAD_TIMEOUT", d.timeout_secs),
        }
    }
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
    /// TRUSTED_APPLICATIONS: whether users may apply at /trusted/request. When off, moderators
    /// grant Trusted through "Change User Class"; existing applications stay readable for them.
    pub applications: bool,
    /// TRUSTED_MIN_UPLOADS: non-remake uploads needed to apply.
    pub min_uploads: i64,
    /// TRUSTED_MIN_DOWNLOADS: total downloads of those uploads needed to apply.
    pub min_downloads: i64,
    /// TRUSTED_REAPPLY_COOLDOWN: days after a rejection before applying again.
    pub reapply_cooldown_days: i64,
}

impl Default for TrustedConfig {
    fn default() -> Self {
        TrustedConfig { applications: true, min_uploads: 10, min_downloads: 10000, reapply_cooldown_days: 90 }
    }
}

impl TrustedConfig {
    fn from_env() -> Self {
        let num = |key: &str, default: i64| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let d = TrustedConfig::default();
        TrustedConfig {
            applications: env::var("TRUSTED_APPLICATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(d.applications),
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
        let site_name = env::var("SITE_NAME").unwrap_or_else(|_| "Nyaa".into());
        let site_flavor = env::var("SITE_FLAVOR").unwrap_or_else(|_| "nyaa".into());
        let tracker_urls: Vec<String> = ["TRACKER_ANNOUNCE_URLS", "TRACKER_EXTRA_URLS"]
            .iter()
            .flat_map(|key| split_list(&env::var(key).unwrap_or_default()))
            .collect();
        let flag = |key: &str, default: bool| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        let non_empty = |key: &str| env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Config {
            captcha: crate::captcha::Captcha::from_env(&secret_key),
            database_url: env::var("DATABASE_URL").unwrap_or_else(|_| "nyaa.db".into()),
            secret_key,
            mfa: crate::auth::mfa::MfaConfig::from_env(&site_name),
            global_site_name: non_empty("GLOBAL_SITE_NAME").unwrap_or_else(|| site_name.clone()),
            site_name,
            sister_site_url: non_empty(if site_flavor == "sukebei" { "EXTERNAL_URL_MAIN" } else { "EXTERNAL_URL_FAP" })
                .map(|url| if url.contains("//") { url } else { format!("//{url}") }),
            site_flavor,
            results_per_page: env::var("RESULTS_PER_PAGE").ok().and_then(|v| v.parse().ok()).unwrap_or(75),
            max_pages: env::var("MAX_PAGES").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            torrent_storage_path: env::var("TORRENT_STORAGE_PATH").unwrap_or_else(|_| "./torrents".into()),
            avatar_storage_path: env::var("AVATAR_STORAGE_PATH").unwrap_or_else(|_| "./avatars".into()),
            enable_gravatar: env::var("ENABLE_GRAVATAR").ok().and_then(|v| v.parse().ok()).unwrap_or(false),
            show_stats: flag("ENABLE_SHOW_STATS", true),
            max_files_view: env::var("MAX_FILES_VIEW").ok().and_then(|v| v.parse().ok()).unwrap_or(1000),
            required_announce_url: flag("ENFORCE_MAIN_ANNOUNCE_URL", false)
                .then(|| non_empty("MAIN_ANNOUNCE_URL").or_else(|| tracker_urls.first().cloned()))
                .flatten(),
            gravatar_url: env::var("GRAVATAR_URL")
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_GRAVATAR_URL.into()),
            gravatar_sha256: match env::var("GRAVATAR_HASH").unwrap_or_default().trim().to_ascii_lowercase().as_str() {
                "" | "md5" => false,
                "sha256" => true,
                other => panic!("GRAVATAR_HASH must be md5 or sha256, got {:?}", other),
            },
            maintenance: MaintenanceConfig::from_env(),
            raid_mode: RaidModeConfig::from_env(),
            registration: RegistrationConfig::from_env(),
            site_url: env::var("SITE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_string(),
            tracker_urls,
            trusted_proxies: crate::utils::proxy::parse_trusted_proxies(
                &env::var("TRUSTED_PROXIES").unwrap_or_default(),
            )
            .unwrap_or_else(|e| panic!("{e}")),
            ratelimit_account_age: env::var("RATELIMIT_ACCOUNT_AGE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(7 * 24 * 3600),
            editing_time_limit: env::var("EDITING_TIME_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(3600),
            upload_limit: UploadLimitConfig::from_env(),
            mail: crate::mail::MailConfig::from_env(),
            meili: crate::search::meili::Meili::from_env(),
            count_cache: crate::search::count_cache::CountCache::from_env(),
            tracker: crate::tracker::Tracker::from_env(),
            trusted: TrustedConfig::from_env(),
            tickets: TicketConfig::from_env(),
            email_blacklist: crate::auth::email_blacklist::EmailBlacklist::from_env(),
        }
    }

    /// Defaults for handler tests: no files, no trackers, no Meilisearch.
    #[cfg(test)]
    pub fn for_tests() -> Config {
        Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            global_site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            sister_site_url: None,
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: String::new(),
            enable_gravatar: false,
            show_stats: true,
            max_files_view: 1000,
            required_announce_url: None,
            gravatar_url: crate::config::DEFAULT_GRAVATAR_URL.into(),
            gravatar_sha256: false,
            maintenance: Default::default(),
            raid_mode: Default::default(),
            registration: Default::default(),
            site_url: String::new(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            count_cache: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            upload_limit: Default::default(),
            mail: Default::default(),
            trusted: Default::default(),
            tickets: Default::default(),
            mfa: Default::default(),
            captcha: None,
            email_blacklist: Default::default(),
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
