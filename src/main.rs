mod cli;
mod config;
mod db;
mod handlers;
mod middleware;
mod models;
mod search;
mod torrent;
mod utils;

use actix_files as fs;
use actix_session::{storage::CookieSessionStore, SessionMiddleware};
use actix_web::{cookie::Key, http::StatusCode, middleware::{ErrorHandlers, Logger}, web, App, HttpServer};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use tera::Tera;

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = cli::run(&args) {
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
            .wrap(ErrorHandlers::new().handler(StatusCode::NOT_FOUND, handlers::site::not_found))
            .wrap(Logger::default())
            .wrap(SessionMiddleware::new(CookieSessionStore::default(), secret_key.clone()))
            // Static files
            .service(fs::Files::new("/static", "./static"))
            // Home / search
            .route("/", web::get().to(handlers::home::home))
            // Info pages
            .route("/rules", web::get().to(handlers::site::rules))
            .route("/help", web::get().to(handlers::site::help))
            // Torrents
            .route("/view/{id}", web::get().to(handlers::torrents::view_torrent))
            .route("/download/{id}", web::get().to(handlers::torrents::download_torrent))
            .route("/magnet/{id}", web::get().to(handlers::torrents::magnet_redirect))
            .route("/upload", web::get().to(handlers::torrents::upload_get))
            .route("/upload", web::post().to(handlers::torrents::upload_post))
            // Users
            .route("/user/{username}", web::get().to(handlers::users::view_user))
            // Account
            .route("/account/login", web::get().to(handlers::account::login_get))
            .route("/account/login", web::post().to(handlers::account::login_post))
            .route("/account/register", web::get().to(handlers::account::register_get))
            .route("/account/register", web::post().to(handlers::account::register_post))
            .route("/account/logout", web::get().to(handlers::account::logout))
            .route("/account/profile", web::get().to(handlers::account::profile))
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
