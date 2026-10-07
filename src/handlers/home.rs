use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::{Banner, Torrent};
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::search::syntax::info_hash;
use crate::utils::context::base_context;
use crate::utils::context::SearchState;
use crate::utils::internal_error;
use crate::utils::pagination::Pagination;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub s: Option<String>,
    pub o: Option<String>,
    pub c: Option<String>,
    pub f: Option<String>,
    pub p: Option<i64>,
}

pub async fn home(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    params: web::Query<SearchParams>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    let is_admin = current_user.as_ref().map(|u| u.is_moderator()).unwrap_or(false);

    let mut q = SearchQuery::from_params(
        params.q.clone(),
        None,
        None,
        params.c.as_deref(),
        params.f.as_deref(),
        params.s.as_deref(),
        params.o.as_deref(),
        params.p,
        cfg.results_per_page,
        is_admin,
    );
    q.viewer_id = current_user.as_ref().map(|u| u.id);

    let mut conn = pool.get().map_err(internal_error)?;

    // A term that is an info hash opens that torrent, as upstream does; deleted ones only
    // for moderators, who are the only ones who could open them
    if let Some(hash) = params.q.as_deref().and_then(info_hash) {
        let torrent = Torrent::by_info_hash(&mut conn, &hash).map_err(internal_error)?;
        if let Some(t) = torrent.filter(|t| is_admin || !(t.is_deleted() || t.is_banned())) {
            return Ok(HttpResponse::Found().insert_header(("Location", format!("/view/{}", t.id))).finish());
        }
    }

    let result = search(&mut conn, cfg.meili.as_ref(), &q).map_err(internal_error)?;

    let pagination = Pagination::new(q.page, result.total, q.per_page);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    let torrents = with_stats(&mut conn, result.torrents).map_err(internal_error)?;
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("banners", &Banner::active(&mut conn).map_err(internal_error)?);
    ctx.insert("search", &SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o));

    let html = tmpl.render("home.html", &ctx).map_err(internal_error)?;

    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{cookie::Key, http::StatusCode, test, App};
    use diesel::r2d2::Pool;
    use diesel::RunQueryDsl;

    #[actix_web::test]
    async fn info_hash_searches_open_the_torrent() {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        {
            let mut conn = pool.get().unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            diesel::sql_query("INSERT INTO users (id, username, password_hash) VALUES (1, 'u', 'x')")
                .execute(&mut conn)
                .unwrap();
            // Torrent 2 is deleted
            diesel::sql_query(
                "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, uploader_id, \
                 main_category_id, sub_category_id) VALUES \
                 (1, X'0123456789abcdef0123456789abcdef01234567', 'One', 't', 0, 1, 1, 1), \
                 (2, X'00000000000000000000000000000000000000ff', 'Two', 't', 32, 1, 1, 1)",
            )
            .execute(&mut conn)
            .unwrap();
        }
        let cfg = Config {
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
            trusted: Default::default(),
        };
        let mut tera = Tera::new("templates/**/*").unwrap();
        crate::utils::tera_filters::register(&mut tera);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(cfg))
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(tera))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/", web::get().to(home)),
        )
        .await;
        let get = |q: &str| test::TestRequest::get().uri(&format!("/?q={q}")).to_request();

        for q in ["0123456789ABCDEF0123456789abcdef01234567", "AERUKZ4JVPG66AJDIVTYTK6N54ASGRLH"] {
            let resp = test::call_service(&app, get(q)).await;
            assert_eq!(resp.status(), StatusCode::FOUND, "{q}");
            assert_eq!(resp.headers().get("Location").unwrap(), "/view/1");
        }
        // Deleted, unknown, or not a hash: an ordinary search
        for q in
            ["00000000000000000000000000000000000000ff", "1123456789abcdef0123456789abcdef01234567", "user%3Au+-two"]
        {
            let resp = test::call_service(&app, get(q)).await;
            assert_eq!(resp.status(), StatusCode::OK, "{q}");
        }
    }
}
