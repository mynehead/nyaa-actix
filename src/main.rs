mod cli;
mod config;
mod db;
mod handlers;
mod middleware;
mod models;
mod search;
mod storage;
mod torrent;
mod utils;

use actix_files as fs;
use actix_session::{storage::CookieSessionStore, SessionMiddleware};
use actix_web::{cookie::Key, http::StatusCode, middleware::{ErrorHandlers, Logger}, web, App, HttpServer};
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

    let storage = storage::Storage::from_env(&cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    log::info!("Storing files on {}", storage.description());
    let storage_data = web::Data::new(storage);

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
            .wrap(ErrorHandlers::new().handler(StatusCode::NOT_FOUND, handlers::site::not_found))
            .wrap(Logger::default())
            .wrap(actix_web::middleware::from_fn(middleware::ip_ban::reject_banned_ip))
            .wrap(SessionMiddleware::new(CookieSessionStore::default(), secret_key.clone()))
            // Static files
            .service(fs::Files::new("/static", "./static"))
            // Home / search
            .route("/", web::get().to(handlers::home::home))
            // Info pages
            .route("/rules", web::get().to(handlers::site::rules))
            .route("/help", web::get().to(handlers::site::help))
            .route("/trusted", web::get().to(handlers::trusted::trusted_info))
            .route("/trusted/request", web::get().to(handlers::trusted::request_trusted))
            .route("/trusted/request", web::post().to(handlers::trusted::request_trusted))
            // Torrents
            .route("/view/{id}", web::get().to(handlers::torrents::view_torrent))
            .service(web::resource("/view/{id}/edit")
                // Room for a full 10 KiB description of percent-encoded non-ASCII text
                .app_data(web::FormConfig::default().limit(256 * 1024))
                .route(web::get().to(handlers::torrents::edit_torrent_get))
                .route(web::post().to(handlers::torrents::edit_torrent_post)))
            .route("/download/{id}", web::get().to(handlers::torrents::download_torrent))
            .route("/magnet/{id}", web::get().to(handlers::torrents::magnet_redirect))
            .route("/upload", web::get().to(handlers::torrents::upload_get))
            .route("/upload", web::post().to(handlers::torrents::upload_post))
            // Users
            .route("/user/{username}", web::get().to(handlers::users::view_user))
            .route("/user/{username}", web::post().to(handlers::users::ban_user_post))
            .route("/user/{username}/nuke/torrents", web::post().to(handlers::users::nuke_torrents_post))
            .route("/user/{username}/nuke/comments", web::post().to(handlers::users::nuke_comments_post))
            // Account
            .route("/account/login", web::get().to(handlers::account::login_get))
            .route("/account/login", web::post().to(handlers::account::login_post))
            .route("/account/register", web::get().to(handlers::account::register_get))
            .route("/account/register", web::post().to(handlers::account::register_post))
            .route("/account/logout", web::get().to(handlers::account::logout))
            .route("/account/profile", web::get().to(handlers::account::profile))
            .route("/account/profile", web::post().to(handlers::account::profile_post))
            .route("/account/profile/avatar", web::post().to(handlers::account::avatar_post))
            .route("/avatar/{id}", web::get().to(handlers::users::avatar))
            // Groups
            .route("/groups", web::get().to(handlers::groups::group_list))
            .route("/groups/create", web::get().to(handlers::groups::create_group_get))
            .route("/groups/create", web::post().to(handlers::groups::create_group_post))
            .route("/group/{slug}", web::get().to(handlers::groups::view_group))
            .route("/group/{slug}/edit", web::get().to(handlers::groups::edit_group_get))
            .route("/group/{slug}/edit", web::post().to(handlers::groups::edit_group_post))
            .route("/group/{slug}/members", web::get().to(handlers::groups::manage_members_get))
            .route("/group/{slug}/members", web::post().to(handlers::groups::manage_members_post))
            // Admin
            .route("/admin/reports", web::get().to(handlers::admin::reports))
            .route("/admin/log", web::get().to(handlers::admin::log))
            .route("/admin/bans", web::get().to(handlers::admin::bans))
            .route("/admin/bans", web::post().to(handlers::admin::bans_post))
            .route("/admin/trusted", web::get().to(handlers::trusted::admin_trusted))
            .route("/admin/trusted/{list_filter}", web::get().to(handlers::trusted::admin_trusted))
            .route("/admin/trusted/application/{id}", web::get().to(handlers::trusted::admin_trusted_application))
            .route("/admin/trusted/application/{id}", web::post().to(handlers::trusted::admin_trusted_application))
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
