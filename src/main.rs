mod auth;
mod cli;
mod config;
mod db;
mod handlers;
mod middleware;
mod models;
mod search;
mod storage;
mod torrent;
mod tracker;
mod utils;

use actix_files as fs;
use actix_session::config::{PersistentSession, TtlExtensionPolicy};
use actix_session::{storage::CookieSessionStore, SessionMiddleware};
use actix_web::{
    cookie::{time::Duration as CookieDuration, Key},
    http::StatusCode,
    middleware::{DefaultHeaders, ErrorHandlers, Logger},
    web, App, HttpServer,
};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use tera::Tera;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = cli::run(&args).await {
        if let Err(e) = result {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let cfg = config::Config::from_env();
    let pool = db::init_pool(&cfg.database_url);

    // Run migrations
    {
        let mut conn = pool.get().expect("Failed to get DB connection");
        db::run_migrations(&mut conn).expect("Failed to run migrations");
    }

    if let Some(meili) = cfg.meili.clone() {
        let every = std::env::var("MEILI_STATS_SYNC_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        log::info!("Searching with Meilisearch index `{}`; syncing tracker stats every {every} s", meili.index());
        search::index::spawn_stats_sync(pool.clone(), meili, std::time::Duration::from_secs(every.max(1)));
    }

    if let Some(tracker) = cfg.tracker.clone() {
        let every = std::env::var("TRACKER_STATS_SYNC_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
        log::info!("Syncing the whitelist with the tracker API at {}; pulling stats every {every} s", tracker.url());
        tracker::spawn_sync(pool.clone(), tracker, std::time::Duration::from_secs(every.max(1)));
    }

    let storage = storage::Storage::from_env(&cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    log::info!("Storing files on {}", storage.description());
    let storage_data = web::Data::new(storage);

    let range_bans = {
        let mut conn = pool.get().expect("Failed to get DB connection");
        web::Data::new(middleware::ip_range_ban::IpRangeBans::load(&mut conn).expect("Failed to load IP range bans"))
    };
    middleware::ip_range_ban::spawn_reload(pool.clone(), range_bans.clone());

    let secret_key = Key::from(cfg.secret_key.as_bytes());
    let cfg_data = web::Data::new(cfg.clone());
    let pool_data = web::Data::new(pool);

    log::info!("Starting {} on http://localhost:{}", cfg.site_name, PORT);

    let server = HttpServer::new(move || {
        let mut tera = Tera::new("templates/**/*").expect("Failed to load templates");
        utils::tera_filters::register(&mut tera);
        let tmpl_data = web::Data::new(tera);

        App::new()
            .app_data(cfg_data.clone())
            .app_data(pool_data.clone())
            .app_data(storage_data.clone())
            .app_data(tmpl_data.clone())
            .app_data(range_bans.clone())
            .wrap(ErrorHandlers::new().handler(StatusCode::NOT_FOUND, handlers::site::not_found))
            .wrap(Logger::default())
            .wrap(actix_web::middleware::from_fn(middleware::ip_ban::reject_banned_ip))
            .wrap(actix_web::middleware::from_fn(middleware::csrf::reject_cross_site))
            .wrap(security_headers())
            // Inside the session middleware too, for its flash message
            .wrap(actix_web::middleware::from_fn(middleware::maintenance::read_only))
            // Registered before the session middleware, so it runs inside it and sees the session
            .wrap(actix_web::middleware::from_fn(middleware::auth::refresh_session))
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    // A 7-day cookie renewed on every request, like upstream; the row in
                    // user_sessions enforces the same limit and can be revoked
                    .session_lifecycle(
                        PersistentSession::default()
                            .session_ttl(CookieDuration::days(middleware::auth::SESSION_TTL_DAYS))
                            .session_ttl_extension_policy(TtlExtensionPolicy::OnEveryRequest),
                    )
                    .build(),
            )
            // Outermost, so a banned network costs no session or database work
            .wrap(actix_web::middleware::from_fn(middleware::ip_range_ban::reject_banned_range))
            // Static files
            .service(fs::Files::new("/static", "./static"))
            // Home / search
            .route("/", web::get().to(handlers::home::home))
            // RSS: also /?page=rss, as upstream; the feed's nyaa: namespace is described at /xmlns/nyaa
            .route("/rss", web::get().to(handlers::home::rss))
            .route("/xmlns/nyaa", web::get().to(handlers::feeds::xmlns_nyaa))
            // JSON API (HTTP Basic auth); upstream serves its v2 upload at both URLs
            .route("/api/info/{query}", web::get().to(handlers::api::info))
            .route("/api/upload", web::post().to(handlers::api::upload))
            .route("/api/v2/upload", web::post().to(handlers::api::upload))
            // Info pages
            .route("/rules", web::get().to(handlers::site::rules))
            .route("/help", web::get().to(handlers::site::help))
            .route("/trusted", web::get().to(handlers::trusted::trusted_info))
            .route("/trusted/request", web::get().to(handlers::trusted::request_trusted))
            .route("/trusted/request", web::post().to(handlers::trusted::request_trusted))
            // Torrents
            .route("/view/{id:\\d+}", web::get().to(handlers::torrents::view_torrent))
            .route("/view/{id:\\d+}", web::post().to(handlers::torrents::post_comment))
            .service(
                web::resource("/view/{id:\\d+}/edit")
                    // Room for a full 10 KiB description of percent-encoded non-ASCII text
                    .app_data(web::FormConfig::default().limit(256 * 1024))
                    .route(web::get().to(handlers::torrents::edit_torrent_get))
                    .route(web::post().to(handlers::torrents::edit_torrent_post)),
            )
            // Upstream's URLs; the .torrent suffix lets clients add a torrent by URL
            .route("/view/{id:\\d+}/comment/{comment_id:\\d+}/edit", web::post().to(handlers::torrents::edit_comment))
            .route(
                "/view/{id:\\d+}/comment/{comment_id:\\d+}/delete",
                web::post().to(handlers::torrents::delete_comment),
            )
            .route("/view/{id:\\d+}/submit_report", web::post().to(handlers::reports::submit_torrent_report))
            .route("/download/{id:\\d+}.torrent", web::get().to(handlers::torrents::download_torrent))
            .route("/view/{id:\\d+}/torrent", web::get().to(handlers::torrents::download_torrent))
            .route("/view/{id:\\d+}/magnet", web::get().to(handlers::torrents::magnet_redirect))
            .route("/download/{id:\\d+}", web::get().to(handlers::torrents::legacy_download_redirect))
            .route("/magnet/{id:\\d+}", web::get().to(handlers::torrents::legacy_magnet_redirect))
            .route("/upload", web::get().to(handlers::torrents::upload_get))
            .route("/upload", web::post().to(handlers::torrents::upload_post))
            // Users
            .route("/user/{username}", web::get().to(handlers::users::view_user))
            .route("/user/{username}", web::post().to(handlers::users::ban_user_post))
            .route("/user/{username}/nuke/torrents", web::post().to(handlers::users::nuke_torrents_post))
            .route("/user/{username}/nuke/comments", web::post().to(handlers::users::nuke_comments_post))
            .route("/user/{username}/comments", web::get().to(handlers::users::view_user_comments))
            // Account
            // Account pages sit at the root like upstream; /account/* redirects for old links
            .route("/login", web::get().to(handlers::account::login_get))
            .route("/login", web::post().to(handlers::account::login_post))
            .route("/register", web::get().to(handlers::account::register_get))
            .route("/register", web::post().to(handlers::account::register_post))
            .route("/logout", web::post().to(handlers::account::logout))
            .route("/profile", web::get().to(handlers::account::profile))
            .route("/profile", web::post().to(handlers::account::profile_post))
            .route("/profile/avatar", web::post().to(handlers::account::avatar_post))
            .route("/account/{page:.+}", web::route().to(handlers::account::legacy_redirect))
            .route("/avatar/{id}", web::get().to(handlers::users::avatar))
            // Support tickets
            .route("/tickets", web::get().to(handlers::tickets::my_tickets))
            .route("/tickets/new", web::get().to(handlers::tickets::new_ticket_get))
            .route("/tickets/new", web::post().to(handlers::tickets::new_ticket_post))
            .route("/tickets/{id:\\d+}", web::get().to(handlers::tickets::view_ticket))
            .route("/tickets/{id:\\d+}", web::post().to(handlers::tickets::ticket_post))
            // Groups
            .route("/groups", web::get().to(handlers::groups::group_list))
            .route("/groups/create", web::get().to(handlers::groups::create_group_get))
            .route("/groups/create", web::post().to(handlers::groups::create_group_post))
            .route("/group/{slug}", web::get().to(handlers::groups::view_group))
            .route("/group/{slug}/edit", web::get().to(handlers::groups::edit_group_get))
            .route("/group/{slug}/edit", web::post().to(handlers::groups::edit_group_post))
            .route("/group/{slug}/members", web::get().to(handlers::groups::manage_members_get))
            .route("/group/{slug}/members", web::post().to(handlers::groups::manage_members_post))
            .route("/group/{slug}/submit_report", web::post().to(handlers::reports::submit_group_report))
            // Admin
            .route("/admin/reports", web::get().to(handlers::reports::admin_reports))
            .route("/admin/reports", web::post().to(handlers::reports::admin_reports_post))
            .route("/admin/tickets", web::get().to(handlers::tickets::admin_tickets))
            .route("/admin/tickets/{list_filter}", web::get().to(handlers::tickets::admin_tickets))
            .route("/admin/log", web::get().to(handlers::admin::log))
            .route("/admin/bans", web::get().to(handlers::admin::bans))
            .route("/admin/bans", web::post().to(handlers::admin::bans_post))
            .route("/admin/bans/ranges", web::post().to(handlers::admin::range_ban_add))
            .route("/admin/bans/ranges/{id}/delete", web::post().to(handlers::admin::range_ban_remove))
            .route("/admin/trusted", web::get().to(handlers::trusted::admin_trusted))
            .route("/admin/trusted/{list_filter}", web::get().to(handlers::trusted::admin_trusted))
            .route("/admin/trusted/application/{id}", web::get().to(handlers::trusted::admin_trusted_application))
            .route("/admin/trusted/application/{id}", web::post().to(handlers::trusted::admin_trusted_application))
            .route("/admin/banners", web::get().to(handlers::banners::list))
            .route("/admin/banners", web::post().to(handlers::banners::create))
            .route("/admin/banners/{id}/edit", web::get().to(handlers::banners::edit_form))
            .route("/admin/banners/{id}/edit", web::post().to(handlers::banners::update))
            .route("/admin/banners/{id}/toggle", web::post().to(handlers::banners::toggle))
            .route("/admin/banners/{id}/delete", web::post().to(handlers::banners::delete))
    })
    .listen(tcp_listener(SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)))?)?;
    // Windows resolves `localhost` to ::1 first and retries a refused connection, so with
    // only IPv4 bound every new browser connection to localhost waited ~300 ms for the
    // fallback to 127.0.0.1. Listen on IPv6 as well, where the host has it.
    let server = match tcp_listener(SocketAddr::from((Ipv6Addr::UNSPECIFIED, PORT))) {
        Ok(lst) => server.listen(lst)?,
        Err(e) => {
            log::warn!("Not listening on IPv6 ([::]:{PORT}): {e}");
            server
        }
    };
    server.run().await
}

const PORT: u16 = 8080;

/// Headers every response gets. The CSP allows inline scripts and styles because the
/// templates (from upstream) use them; it still blocks scripts from other hosts, plugins,
/// framing and form posts to other sites.
fn security_headers() -> DefaultHeaders {
    DefaultHeaders::new()
        .add((
            "Content-Security-Policy",
            "default-src 'self'; \
             script-src 'self' 'unsafe-inline' https://cdnjs.cloudflare.com; \
             style-src 'self' 'unsafe-inline' https://cdnjs.cloudflare.com; \
             font-src 'self' data: https://cdnjs.cloudflare.com; \
             img-src * data:; \
             object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'",
        ))
        .add(("X-Frame-Options", "DENY"))
        .add(("X-Content-Type-Options", "nosniff"))
        .add(("Referrer-Policy", "same-origin"))
}

/// A listening socket like the one `HttpServer::bind` makes, except that an IPv6 one
/// takes IPv6 only, so it can sit next to the IPv4 one on the same port on every OS.
fn tcp_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // On Windows SO_REUSEADDR would let a second server take the port silently
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_and_ipv6_listeners_share_a_port() {
        let v4 = tcp_listener(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).unwrap();
        let port = v4.local_addr().unwrap().port();
        match tcp_listener(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))) {
            Ok(v6) => assert_eq!(v6.local_addr().unwrap().port(), port),
            // Hosts without IPv6 can't make the socket at all; the server warns and goes on
            // (EAFNOSUPPORT on Linux, WSAEAFNOSUPPORT on Windows)
            Err(e) if matches!(e.raw_os_error(), Some(97 | 10047)) => {}
            Err(e) => panic!("IPv6 listener next to IPv4 on port {port}: {e}"),
        }
    }
}
