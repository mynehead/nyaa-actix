mod config;
mod db;
mod handlers;
mod middleware;
mod models;
mod search;
mod torrent;
mod utils;

use actix_files as fs;
use actix_session::config::{PersistentSession, TtlExtensionPolicy};
use actix_session::{storage::CookieSessionStore, SessionExt, SessionMiddleware};
use actix_web::cookie::{time::Duration, Key};
use actix_web::dev::Service;
use actix_web::{middleware::Logger, web, App, HttpServer};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use tera::Tera;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let cfg = config::Config::from_env();
    let pool = db::init_pool(&cfg.database_url);

    // Run migrations
    {
        let mut conn = pool.get().expect("Failed to get DB connection");
        conn.run_pending_migrations(MIGRATIONS).expect("Failed to run migrations");
    }

    let secret_key = Key::from(cfg.secret_key.as_bytes());
    let cfg_data = web::Data::new(cfg.clone());
    let pool_data = web::Data::new(pool);

    let bind_addr = "0.0.0.0:8080";
    log::info!("Starting {} on http://{}", cfg.site_name, bind_addr);

    HttpServer::new(move || {
        let mut tera = Tera::new("templates/**/*").expect("Failed to load templates");
        utils::tera_filters::register(&mut tera);
        let tmpl_data = web::Data::new(tera);

        App::new()
            .app_data(cfg_data.clone())
            .app_data(pool_data.clone())
            .app_data(tmpl_data.clone())
            // Runs inside the session middleware (registered after it), so the session is loaded
            .wrap_fn(|req, srv| {
                let sid = req.get_session()
                    .get::<String>(middleware::auth::SESSION_ID_KEY).ok().flatten();
                let pool = req.app_data::<web::Data<db::DbPool>>().cloned();
                let behind_proxy = req.app_data::<web::Data<config::Config>>()
                    .map(|c| c.behind_reverse_proxy).unwrap_or(false);
                let ip = utils::client_ip(req.request(), behind_proxy);
                let response = srv.call(req);
                async move {
                    if let (Some(sid), Some(pool)) = (sid, pool) {
                        let touched = web::block(move || -> anyhow::Result<()> {
                            let mut conn = pool.get()?;
                            middleware::auth::touch_session(&mut conn, &sid, ip)?;
                            Ok(())
                        }).await;
                        if let Ok(Err(e)) = touched {
                            log::error!("Failed to update session: {:#}", e);
                        }
                    }
                    response.await
                }
            })
            .wrap(Logger::default())
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    // A 7-day cookie renewed on every request, like upstream; the server-side
                    // row in user_sessions enforces the same limit and can be revoked.
                    .session_lifecycle(
                        PersistentSession::default()
                            .session_ttl(Duration::days(middleware::auth::SESSION_TTL_DAYS))
                            .session_ttl_extension_policy(TtlExtensionPolicy::OnEveryRequest),
                    )
                    .build(),
            )
            // Static files
            .service(fs::Files::new("/static", "./static").show_files_listing())
            // Home / search
            .route("/", web::get().to(handlers::home::home))
            // Torrents (upstream URLs; the .torrent suffix lets clients add a torrent by URL)
            .route("/view/{id:\\d+}", web::get().to(handlers::torrents::view_torrent))
            .route("/download/{id:\\d+}.torrent", web::get().to(handlers::torrents::download_torrent))
            .route("/view/{id:\\d+}/torrent", web::get().to(handlers::torrents::download_torrent))
            .route("/view/{id:\\d+}/magnet", web::get().to(handlers::torrents::magnet_redirect))
            .route("/download/{id:\\d+}", web::get().to(handlers::torrents::legacy_download_redirect))
            .route("/magnet/{id:\\d+}", web::get().to(handlers::torrents::legacy_magnet_redirect))
            .route("/upload", web::get().to(handlers::torrents::upload_get))
            .route("/upload", web::post().to(handlers::torrents::upload_post))
            // Users
            .route("/user/{username}", web::get().to(handlers::users::view_user))
            // Account (at the root like upstream; /account/* redirects for old links)
            .route("/login", web::get().to(handlers::account::login_get))
            .route("/login", web::post().to(handlers::account::login_post))
            .route("/register", web::get().to(handlers::account::register_get))
            .route("/register", web::post().to(handlers::account::register_post))
            .route("/logout", web::get().to(handlers::account::logout))
            .route("/profile", web::get().to(handlers::account::profile))
            .route("/account/{page}", web::route().to(handlers::account::legacy_redirect))
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
    })
    .bind(bind_addr)?
    .run()
    .await
}
