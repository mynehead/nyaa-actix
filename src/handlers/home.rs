use actix_web::http::StatusCode;
use actix_web::{web, HttpResponse, Result};
use tera::Tera;

use crate::auth::{CurrentUser, Permission};
use crate::config::Config;
use crate::db::DbPool;
use crate::models::{Banner, Torrent, User};
use crate::search::db::{with_stats, ListParams, ListedTorrent, SearchQuery};
use crate::search::search;
use crate::search::syntax::info_hash;
use crate::utils::context::base_context;
use crate::utils::context::SearchState;
use crate::utils::pagination::Pagination;
use crate::utils::{html_page, redirect, with_conn};

/// The listing's query parameters. Like upstream, each has older aliases (`term`, `cats`,
/// `filter`, `user`, `page`, `offset`), the first one present wins, and `page=rss`,
/// `magnets` or `m` ask for the RSS feed and magnet links in it.
#[derive(Debug, Default)]
pub struct SearchParams {
    pub list: ListParams,
    pub u: Option<String>,
    pub rss: bool,
    pub magnets: bool,
}

impl SearchParams {
    fn from_pairs(pairs: &[(String, String)]) -> Self {
        let get =
            |keys: &[&str]| keys.iter().find_map(|k| pairs.iter().find(|(name, _)| name == k).map(|(_, v)| v.clone()));
        let has = |key: &str| pairs.iter().any(|(name, _)| name == key);
        SearchParams {
            list: ListParams {
                q: get(&["q", "term"]),
                s: get(&["s"]),
                o: get(&["o"]),
                c: get(&["c", "cats"]),
                f: get(&["f", "filter"]),
                // "page=rss" is no page number; like upstream that means page 1
                p: get(&["p", "page", "offset"]).and_then(|p| p.parse().ok()),
            },
            u: get(&["u", "user"]).filter(|u| !u.is_empty()),
            rss: get(&["page"]).as_deref() == Some("rss"),
            magnets: has("magnets") || has("m"),
        }
    }

    /// The feed for this listing: upstream's navbar RSS link keeps the term, category,
    /// filter and uploader, and drops sorting and paging.
    fn rss_url(&self) -> String {
        let mut url = "/?page=rss".to_string();
        for (key, value) in [("q", &self.list.q), ("c", &self.list.c), ("f", &self.list.f), ("u", &self.u)] {
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

/// What the listing's database work produced: the feed, or one page's data.
enum Listing {
    Rss(String),
    Page { torrents: Vec<ListedTorrent>, total: i64, banners: Vec<Banner> },
}

async fn listing(
    CurrentUser(current_user): CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    params: SearchParams,
) -> Result<HttpResponse> {
    let moderator = current_user.as_ref().is_some_and(|u| u.can(Permission::ModerateTorrents));

    // `u=name` lists one uploader's torrents, as their profile does; an unknown name is a 404
    let uploader = match params.u.clone() {
        Some(name) => Some(
            with_conn(&pool, move |conn| User::by_username(conn, &name))
                .await?
                .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?,
        ),
        None => None,
    };

    let mut q =
        SearchQuery::from_params(&params.list, uploader.as_ref().map(|u| u.id), None, cfg.results_per_page, moderator);
    q.viewer_id = current_user.as_ref().map(|u| u.id);
    if let Some(uploader) = &uploader {
        q.hide_anonymous = !(moderator || q.viewer_id == Some(uploader.id));
    }

    // A term that is an info hash opens that torrent, as upstream does; deleted ones only
    // for moderators, who are the only ones who could open them
    if let Some(hash) = params.list.q.as_deref().and_then(info_hash).filter(|_| !params.rss) {
        let torrent = with_conn(&pool, move |conn| Torrent::by_info_hash(conn, &hash)).await?;
        if let Some(t) = torrent.filter(|t| moderator || !(t.is_deleted() || t.is_banned())) {
            return Ok(redirect(&format!("/view/{}", t.id)));
        }
    }

    crate::utils::pagination::check_max_pages(q.page, cfg.max_pages)?;
    let rss = params.rss.then(|| {
        let label = match params.list.q.as_deref().filter(|t| !t.is_empty()) {
            Some(term) => format!("\"{term}\""),
            None => "Home".to_string(),
        };
        (label, params.magnets)
    });
    let (page, per_page) = (q.page, q.per_page);
    let (db_cfg, db_tmpl) = (cfg.clone(), tmpl.clone());
    let listing = with_conn(&pool, move |conn| -> anyhow::Result<Listing> {
        let result = search(conn, &db_cfg, &q)?;
        let torrents = with_stats(conn, result.torrents)?;
        if let Some((label, magnets)) = rss {
            let xml = crate::handlers::feeds::render_rss(conn, &db_tmpl, &db_cfg, &label, &torrents, magnets)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            return Ok(Listing::Rss(xml));
        }
        Ok(Listing::Page { torrents, total: result.total, banners: Banner::active(conn)? })
    })
    .await?;
    let (torrents, total, banners) = match listing {
        Listing::Rss(xml) => {
            return Ok(HttpResponse::Ok()
                .content_type("application/xml")
                // Upstream caches feeds for five minutes
                .insert_header(("Cache-Control", "max-age=300"))
                .body(xml));
        }
        Listing::Page { torrents, total, banners } => (torrents, total, banners),
    };

    // Upstream's hint: when the first word of a search names a user, link to the rest of
    // the search within that user's torrents (not when a `u=` filter is already active)
    let first_word = params.list.q.as_deref().filter(|_| params.u.is_none()).and_then(split_first_word);
    let first_word_user = match first_word {
        Some((word, rest)) => {
            let word = word.to_string();
            with_conn(&pool, move |conn| User::by_username_ignoring_case(conn, &word)).await?.map(|user| {
                let mut url = format!("/user/{}?q={}", user.username, urlencoding::encode(rest));
                let l = &params.list;
                for (key, value) in [("c", &l.c), ("f", &l.f), ("s", &l.s), ("o", &l.o)] {
                    if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
                        url.push_str(&format!("&{key}={}", urlencoding::encode(value)));
                    }
                }
                serde_json::json!({ "username": user.username, "url": url })
            })
        }
        None => None,
    };

    let pagination = Pagination::capped(page, total, per_page, cfg.max_pages);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("banners", &banners);
    let search = SearchState { user: params.u.clone().unwrap_or_default(), ..SearchState::new(&params.list) };
    ctx.insert("search", &search);
    ctx.insert("rss_url", &params.rss_url());
    ctx.insert("first_word_user", &first_word_user);

    html_page(&tmpl, "home.html", &ctx, StatusCode::OK)
}

/// Splits a search into its first word, if that could be a username (upstream's
/// `^([a-zA-Z0-9_-]+) *(.*|$)`, but the word must end at a space so `user:x` isn't "user"),
/// and the rest.
fn split_first_word(term: &str) -> Option<(&str, &str)> {
    let (word, rest) = term.split_once(' ').unwrap_or((term, ""));
    let valid = !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    valid.then(|| (word, rest.trim_start_matches(' ')))
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
            (p.list.q.as_deref(), p.list.c.as_deref(), p.list.f.as_deref(), p.u.as_deref(), p.list.p, p.rss),
            (Some("a b"), Some("1_2"), Some("2"), Some("bob"), Some(3), false)
        );
        assert_eq!(p.rss_url(), "/?page=rss&q=a%20b&c=1_2&f=2&u=bob");
        // The short names win; "page=rss" is the feed, on page 1 unless p says otherwise
        let p = parse("q=x&term=y&page=rss&m");
        assert_eq!((p.list.q.as_deref(), p.list.p, p.rss, p.magnets), (Some("x"), None, true, true));
        let p = parse("page=rss&p=2&magnets=");
        assert_eq!((p.list.p, p.rss, p.magnets), (Some(2), true, true));
        assert_eq!(parse("u=").u, None);
        assert_eq!(parse("").rss_url(), "/?page=rss");
    }

    #[::core::prelude::v1::test]
    fn first_word_of_a_search() {
        assert_eq!(split_first_word("Erai-raws  One Piece"), Some(("Erai-raws", "One Piece")));
        assert_eq!(split_first_word("bob"), Some(("bob", "")));
        assert_eq!(split_first_word("user:bob x"), None);
        assert_eq!(split_first_word(" bob"), None);
    }

    #[actix_web::test]
    async fn info_hash_searches_open_the_torrent() {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(&crate::db::test_url())).unwrap();
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
                 (2, X'00000000000000000000000000000000000000ff', 'Two', 't', 32, 1, 1, 1), \
                 (3, X'00000000000000000000000000000000000000aa', 'Three', 't', 0, 1, 1, 1), \
                 (4, X'00000000000000000000000000000000000000bb', 'Four', 't', 0, 1, 1, 1)",
            )
            .execute(&mut conn)
            .unwrap();
        }
        let cfg = Config { results_per_page: 1, max_pages: 2, ..Config::for_tests() };
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

        // The first word names a user (ignoring case): link to the rest within their torrents
        let hint = "Click here to see only results uploaded by";
        for (uri, link) in [
            ("/?q=U+%20two&c=1_0&s=size", Some("&#x2F;user&#x2F;u?q=two&amp;c=1_0&amp;s=size")),
            ("/?q=u", Some("&#x2F;user&#x2F;u?q=")),
            ("/?q=u+two&u=u", None),
            ("/?q=user%3Au+two", None),
            ("/?q=nobody+two", None),
        ] {
            let req = test::TestRequest::get().uri(uri).to_request();
            let body = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
            assert_eq!(body.contains(hint), link.is_some(), "{uri}");
            assert!(link.is_none_or(|l| body.contains(&format!(r#"{l}">{hint} u</a>"#))), "{uri}: {body}");
        }

        // MAX_PAGES (2 here): three torrents at one a page, but only two pages are linked
        let page = |p: u32| test::TestRequest::get().uri(&format!("/?f=0&p={p}")).to_request();
        let body = String::from_utf8(test::call_and_read_body(&app, page(1)).await.to_vec()).unwrap();
        assert!(body.contains("p=2") && !body.contains("p=3"), "{body}");
        assert_eq!(test::call_service(&app, page(2)).await.status(), StatusCode::OK);
        assert_eq!(test::call_service(&app, page(3)).await.status(), StatusCode::NOT_FOUND);
    }
}
