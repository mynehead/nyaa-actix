use actix_web::{web, HttpResponse, Result};
use tera::Tera;

use crate::auth::{CurrentUser, Permission};
use crate::config::Config;
use crate::db::DbPool;
use crate::models::{Banner, Torrent, User};
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::search::syntax::info_hash;
use crate::utils::context::base_context;
use crate::utils::context::SearchState;
use crate::utils::internal_error;
use crate::utils::pagination::Pagination;

/// The listing's query parameters. Like upstream, each has older aliases (`term`, `cats`,
/// `filter`, `user`, `page`, `offset`), the first one present wins, and `page=rss`,
/// `magnets` or `m` ask for the RSS feed and magnet links in it.
#[derive(Debug, Default)]
pub struct SearchParams {
    pub q: Option<String>,
    pub s: Option<String>,
    pub o: Option<String>,
    pub c: Option<String>,
    pub f: Option<String>,
    pub u: Option<String>,
    pub p: Option<i64>,
    pub rss: bool,
    pub magnets: bool,
}

impl SearchParams {
    fn from_pairs(pairs: &[(String, String)]) -> Self {
        let get =
            |keys: &[&str]| keys.iter().find_map(|k| pairs.iter().find(|(name, _)| name == k).map(|(_, v)| v.clone()));
        let has = |key: &str| pairs.iter().any(|(name, _)| name == key);
        SearchParams {
            q: get(&["q", "term"]),
            s: get(&["s"]),
            o: get(&["o"]),
            c: get(&["c", "cats"]),
            f: get(&["f", "filter"]),
            u: get(&["u", "user"]).filter(|u| !u.is_empty()),
            // "page=rss" is no page number; like upstream that means page 1
            p: get(&["p", "page", "offset"]).and_then(|p| p.parse().ok()),
            rss: get(&["page"]).as_deref() == Some("rss"),
            magnets: has("magnets") || has("m"),
        }
    }

    /// The feed for this listing: upstream's navbar RSS link keeps the term, category,
    /// filter and uploader, and drops sorting and paging.
    fn rss_url(&self) -> String {
        let mut url = "/?page=rss".to_string();
        for (key, value) in [("q", &self.q), ("c", &self.c), ("f", &self.f), ("u", &self.u)] {
            if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                url.push_str(&format!("&{key}={}", urlencoding::encode(value)));
            }
        }
        url
    }
}

pub async fn home(
    current_user: CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<Vec<(String, String)>>,
) -> Result<HttpResponse> {
    let params = SearchParams::from_pairs(&query);
    listing(current_user, pool, tmpl, cfg, params).await
}

/// `/rss`: the same listing as `/?page=rss`.
pub async fn rss(
    current_user: CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<Vec<(String, String)>>,
) -> Result<HttpResponse> {
    let params = SearchParams { rss: true, ..SearchParams::from_pairs(&query) };
    listing(current_user, pool, tmpl, cfg, params).await
}

async fn listing(
    CurrentUser(current_user): CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    params: SearchParams,
) -> Result<HttpResponse> {
    let moderator = current_user.as_ref().is_some_and(|u| u.can(Permission::ModerateTorrents));

    let mut conn = pool.get().map_err(internal_error)?;

    // `u=name` lists one uploader's torrents, as their profile does; an unknown name is a 404
    let uploader = match params.u.as_deref() {
        Some(name) => Some(
            User::by_username(&mut conn, name)
                .map_err(internal_error)?
                .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?,
        ),
        None => None,
    };

    let mut q = SearchQuery::from_params(
        params.q.clone(),
        uploader.as_ref().map(|u| u.id),
        None,
        params.c.as_deref(),
        params.f.as_deref(),
        params.s.as_deref(),
        params.o.as_deref(),
        params.p,
        cfg.results_per_page,
        moderator,
    );
    q.viewer_id = current_user.as_ref().map(|u| u.id);
    if let Some(uploader) = &uploader {
        q.hide_anonymous = !(moderator || q.viewer_id == Some(uploader.id));
    }

    // A term that is an info hash opens that torrent, as upstream does; deleted ones only
    // for moderators, who are the only ones who could open them
    if let Some(hash) = params.q.as_deref().and_then(info_hash).filter(|_| !params.rss) {
        let torrent = Torrent::by_info_hash(&mut conn, &hash).map_err(internal_error)?;
        if let Some(t) = torrent.filter(|t| moderator || !(t.is_deleted() || t.is_banned())) {
            return Ok(HttpResponse::Found().insert_header(("Location", format!("/view/{}", t.id))).finish());
        }
    }

    let result = search(&mut conn, cfg.meili.as_ref(), &q).map_err(internal_error)?;
    let torrents = with_stats(&mut conn, result.torrents).map_err(internal_error)?;

    if params.rss {
        let label = match params.q.as_deref().filter(|t| !t.is_empty()) {
            Some(term) => format!("\"{term}\""),
            None => "Home".to_string(),
        };
        let xml = crate::handlers::feeds::render_rss(&mut conn, &tmpl, &cfg, &label, &torrents, params.magnets)?;
        return Ok(HttpResponse::Ok()
            .content_type("application/xml")
            // Upstream caches feeds for five minutes
            .insert_header(("Cache-Control", "max-age=300"))
            .body(xml));
    }

    let pagination = Pagination::new(q.page, result.total, q.per_page);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("banners", &Banner::active(&mut conn).map_err(internal_error)?);
    let search = SearchState {
        user: params.u.clone().unwrap_or_default(),
        ..SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o)
    };
    ctx.insert("search", &search);
    ctx.insert("rss_url", &params.rss_url());

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

    #[::core::prelude::v1::test]
    fn upstream_param_aliases() {
        let parse =
            |query: &str| SearchParams::from_pairs(&web::Query::<Vec<(String, String)>>::from_query(query).unwrap());
        let p = parse("term=a+b&cats=1_2&filter=2&user=bob&offset=3");
        assert_eq!(
            (p.q.as_deref(), p.c.as_deref(), p.f.as_deref(), p.u.as_deref(), p.p, p.rss),
            (Some("a b"), Some("1_2"), Some("2"), Some("bob"), Some(3), false)
        );
        assert_eq!(p.rss_url(), "/?page=rss&q=a%20b&c=1_2&f=2&u=bob");
        // The short names win; "page=rss" is the feed, on page 1 unless p says otherwise
        let p = parse("q=x&term=y&page=rss&m");
        assert_eq!((p.q.as_deref(), p.p, p.rss, p.magnets), (Some("x"), None, true, true));
        let p = parse("page=rss&p=2&magnets=");
        assert_eq!((p.p, p.rss, p.magnets), (Some(2), true, true));
        assert_eq!(parse("u=").u, None);
        assert_eq!(parse("").rss_url(), "/?page=rss");
    }

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
            ratelimit_account_age: 0,
            meili: None,
            tracker: None,
            trusted: Default::default(),
            tickets: Default::default(),
            mfa: Default::default(),
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
