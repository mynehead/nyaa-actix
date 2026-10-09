//! Admin > Banners: site-wide notices shown above the torrent list on the main page.

use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::Connection;
use serde::Deserialize;
use tera::Tera;

use crate::auth::Moderator;
use crate::config::Config;
use crate::db::DbPool;
use crate::models::{AdminLog, Banner};
use crate::utils::context::base_context;
use crate::utils::{flash, internal_error};

const MAX_LENGTH: usize = 1024;

fn back_to_list() -> HttpResponse {
    HttpResponse::SeeOther().insert_header(("Location", "/admin/banners")).finish()
}

pub async fn list(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let banners = Banner::all_with_creator(&mut conn).map_err(internal_error)?;
    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("banners", &banners);
    let html = tmpl.render("admin/banners.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[derive(Debug, Deserialize)]
pub struct BannerForm {
    pub content: String,
}

/// The trimmed content, or the reason it can't be saved.
fn validate(content: &str) -> std::result::Result<&str, String> {
    let content = content.trim();
    if content.is_empty() {
        return Err("The content is empty.".into());
    }
    if content.chars().count() > MAX_LENGTH {
        return Err(format!("The content is longer than {} characters.", MAX_LENGTH));
    }
    Ok(content)
}

pub async fn create(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    form: web::Form<BannerForm>,
) -> Result<HttpResponse> {
    let content = match validate(&form.content) {
        Ok(content) => content,
        Err(reason) => {
            flash::push(&session, "danger", "Banner not saved!", &reason);
            return Ok(back_to_list());
        }
    };
    let mut conn = pool.get().map_err(internal_error)?;
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        Banner::create(conn, content, user.id)?;
        AdminLog::add(conn, user.id, "Created banner")
    })
    .map_err(internal_error)?;
    flash::push(&session, "success", "Banner created.", "");
    Ok(back_to_list())
}

pub async fn edit_form(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let banner = Banner::find(&mut conn, path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("No such banner"))?;
    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("banner", &banner);
    let html = tmpl.render("admin/banner_edit.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn update(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<i32>,
    form: web::Form<BannerForm>,
) -> Result<HttpResponse> {
    let id = path.into_inner();
    let content = match validate(&form.content) {
        Ok(content) => content,
        Err(reason) => {
            flash::push(&session, "danger", "Banner not saved!", &reason);
            return Ok(HttpResponse::SeeOther()
                .insert_header(("Location", format!("/admin/banners/{}/edit", id)))
                .finish());
        }
    };
    let mut conn = pool.get().map_err(internal_error)?;
    let updated = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            let updated = Banner::update_content(conn, id, content)?;
            if updated {
                AdminLog::add(conn, user.id, &format!("Edited banner #{}", id))?;
            }
            Ok(updated)
        })
        .map_err(internal_error)?;
    if !updated {
        return Err(actix_web::error::ErrorNotFound("No such banner"));
    }
    flash::push(&session, "success", &format!("Banner #{} updated.", id), "");
    Ok(back_to_list())
}

pub async fn toggle(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;
    let state = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            let Some(active) = Banner::toggle(conn, id)? else { return Ok(None) };
            let state = if active { "activated" } else { "deactivated" };
            AdminLog::add(conn, user.id, &format!("Banner #{} {}", id, state))?;
            Ok(Some(state))
        })
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("No such banner"))?;
    flash::push(&session, "success", &format!("Banner #{} {}.", id, state), "");
    Ok(back_to_list())
}

pub async fn delete(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;
    let deleted = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            let deleted = Banner::delete(conn, id)?;
            if deleted {
                AdminLog::add(conn, user.id, &format!("Deleted banner #{}", id))?;
            }
            Ok(deleted)
        })
        .map_err(internal_error)?;
    if !deleted {
        return Err(actix_web::error::ErrorNotFound("No such banner"));
    }
    flash::push(&session, "success", &format!("Deleted banner #{}.", id), "");
    Ok(back_to_list())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{
        cookie::{Cookie, Key},
        http::StatusCode,
        test, App,
    };
    use diesel::r2d2::Pool;
    use diesel::RunQueryDsl;

    fn pool() -> DbPool {
        // One connection, so every request sees the same in-memory database
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
                           (1, 'user', 'x', 1, 0), (3, 'mod', 'x', 1, 2)",
        )
        .execute(&mut conn)
        .unwrap();
        pool
    }

    fn config() -> Config {
        Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: String::new(),
            enable_gravatar: false,
            maintenance: Default::default(),
            site_url: String::new(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            upload_limit: Default::default(),
            trusted: Default::default(),
            tickets: Default::default(),
        }
    }

    use crate::middleware::auth::test_support::login;

    macro_rules! app {
        ($pool:expr, $user:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            let app = test::init_service(
                App::new()
                    .app_data(web::Data::new(config()))
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(tera))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/login/{id}", web::get().to(login))
                    .route("/", web::get().to(crate::handlers::home::home))
                    .route("/admin/banners", web::get().to(list))
                    .route("/admin/banners", web::post().to(create))
                    .route("/admin/banners/{id}/edit", web::get().to(edit_form))
                    .route("/admin/banners/{id}/edit", web::post().to(update))
                    .route("/admin/banners/{id}/toggle", web::post().to(toggle))
                    .route("/admin/banners/{id}/delete", web::post().to(delete)),
            )
            .await;
            let cookie: Option<Cookie<'static>> = match $user {
                Some(id) => {
                    let res =
                        test::call_service(&app, test::TestRequest::get().uri(&format!("/login/{}", id)).to_request())
                            .await;
                    res.response().cookies().next().map(|c| c.into_owned())
                }
                None => None,
            };
            (app, cookie)
        }};
    }

    fn get(uri: &str, cookie: &Option<Cookie<'static>>) -> test::TestRequest {
        let mut req = test::TestRequest::get().uri(uri);
        if let Some(c) = cookie {
            req = req.cookie(c.clone());
        }
        req
    }

    fn post(uri: &str, cookie: &Option<Cookie<'static>>, form: &[(&str, &str)]) -> test::TestRequest {
        let mut req = test::TestRequest::post().uri(uri).set_form(form);
        if let Some(c) = cookie {
            req = req.cookie(c.clone());
        }
        req
    }

    /// The session cookie a response sets (flashes live in it), or the one sent.
    fn next_cookie(res: &actix_web::dev::ServiceResponse, sent: &Option<Cookie<'static>>) -> Option<Cookie<'static>> {
        res.response().cookies().next().map(|c| c.into_owned()).or_else(|| sent.clone())
    }

    #[actix_web::test]
    async fn only_moderators_manage_banners() {
        let pool = pool();
        for (user, status) in [(None, StatusCode::UNAUTHORIZED), (Some(1), StatusCode::FORBIDDEN)] {
            let (app, cookie) = app!(pool, user);
            let res = test::call_service(&app, get("/admin/banners", &cookie).to_request()).await;
            assert_eq!(res.status(), status, "{user:?}");
            let res =
                test::call_service(&app, post("/admin/banners", &cookie, &[("content", "Hi")]).to_request()).await;
            assert_eq!(res.status(), status, "{user:?}");
        }
        assert!(Banner::all_with_creator(&mut pool.get().unwrap()).unwrap().is_empty());
    }

    #[actix_web::test]
    async fn create_toggle_and_delete_banner() {
        let pool = pool();
        let (app, cookie) = app!(pool, Some(3));
        let home = |body: actix_web::web::Bytes| String::from_utf8(body.to_vec()).unwrap();

        let res = test::call_service(
            &app,
            post("/admin/banners", &cookie, &[("content", "  <b>Maintenance</b> tonight  ")]).to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let page =
            home(test::call_and_read_body(&app, get("/admin/banners", &next_cookie(&res, &cookie)).to_request()).await);
        assert!(page.contains("Banner created."), "{page}");
        assert!(page.contains("&lt;b&gt;Maintenance&lt;&#x2F;b&gt; tonight</td>"), "{page}");
        assert!(page.contains("label-success\">Active"), "{page}");
        assert!(page.contains("<a href=\"/user/mod\">mod</a>"), "{page}");
        assert!(page.contains("href=\"/admin/banners\">Banners</a>"), "nav item");

        // Shown to everyone on the main page, escaped
        let (anon, none) = app!(pool, None::<i32>);
        let main = home(test::call_and_read_body(&anon, get("/", &none).to_request()).await);
        assert!(main.contains("data-banner-id=\"1\""), "{main}");
        assert!(main.contains("&lt;b&gt;Maintenance"), "{main}");

        let res = test::call_service(&app, post("/admin/banners/1/toggle", &cookie, &[]).to_request()).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let page =
            home(test::call_and_read_body(&app, get("/admin/banners", &next_cookie(&res, &cookie)).to_request()).await);
        assert!(page.contains("Banner #1 deactivated."), "{page}");
        assert!(page.contains(">Activate</button>"), "{page}");
        let main = home(test::call_and_read_body(&anon, get("/", &none).to_request()).await);
        assert!(!main.contains("site-banner"), "{main}");

        let res = test::call_service(&app, post("/admin/banners/1/delete", &cookie, &[]).to_request()).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(Banner::all_with_creator(&mut pool.get().unwrap()).unwrap().is_empty());
        let res = test::call_service(&app, post("/admin/banners/1/delete", &cookie, &[]).to_request()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let (log, _) = AdminLog::page(&mut pool.get().unwrap(), 1, 10).unwrap();
        let log: Vec<&str> = log.iter().rev().map(|e| e.entry.log.as_str()).collect();
        assert_eq!(log, ["Created banner", "Banner #1 deactivated", "Deleted banner #1"]);
        assert!(AdminLog::page(&mut pool.get().unwrap(), 1, 10).unwrap().0.iter().all(|e| e.admin_name == "mod"));
    }

    #[actix_web::test]
    async fn empty_banner_is_rejected() {
        let pool = pool();
        let (app, cookie) = app!(pool, Some(3));
        let res = test::call_service(&app, post("/admin/banners", &cookie, &[("content", "   ")]).to_request()).await;
        let cookie = next_cookie(&res, &cookie);
        let page = String::from_utf8(
            test::call_and_read_body(&app, get("/admin/banners", &cookie).to_request()).await.to_vec(),
        )
        .unwrap();
        assert!(page.contains("The content is empty."), "{page}");
        assert!(page.contains("No banners."), "{page}");
    }

    #[actix_web::test]
    async fn edit_banner() {
        let pool = pool();
        let (app, cookie) = app!(pool, Some(3));
        let body = |b: actix_web::web::Bytes| String::from_utf8(b.to_vec()).unwrap();
        test::call_service(&app, post("/admin/banners", &cookie, &[("content", "Old text")]).to_request()).await;

        // Regular users and guests can't edit
        for (user, status) in [(None, StatusCode::UNAUTHORIZED), (Some(1), StatusCode::FORBIDDEN)] {
            let (other, c) = app!(pool, user);
            let res = test::call_service(&other, get("/admin/banners/1/edit", &c).to_request()).await;
            assert_eq!(res.status(), status, "{user:?}");
            let res =
                test::call_service(&other, post("/admin/banners/1/edit", &c, &[("content", "Hacked")]).to_request())
                    .await;
            assert_eq!(res.status(), status, "{user:?}");
        }

        let page = body(test::call_and_read_body(&app, get("/admin/banners", &cookie).to_request()).await);
        assert!(page.contains("href=\"/admin/banners/1/edit\""), "{page}");
        let form = body(test::call_and_read_body(&app, get("/admin/banners/1/edit", &cookie).to_request()).await);
        assert!(form.contains(">Old text</textarea>"), "{form}");

        let res =
            test::call_service(&app, post("/admin/banners/1/edit", &cookie, &[("content", "   ")]).to_request()).await;
        assert_eq!(res.headers().get("Location").unwrap(), "/admin/banners/1/edit");
        let form = body(
            test::call_and_read_body(&app, get("/admin/banners/1/edit", &next_cookie(&res, &cookie)).to_request())
                .await,
        );
        assert!(form.contains("The content is empty."), "{form}");
        assert!(form.contains(">Old text</textarea>"), "{form}");

        let res = test::call_service(
            &app,
            post("/admin/banners/1/edit", &cookie, &[("content", "  New text  ")]).to_request(),
        )
        .await;
        assert_eq!(res.headers().get("Location").unwrap(), "/admin/banners");
        let page =
            body(test::call_and_read_body(&app, get("/admin/banners", &next_cookie(&res, &cookie)).to_request()).await);
        assert!(page.contains("Banner #1 updated."), "{page}");
        let banner = Banner::find(&mut pool.get().unwrap(), 1).unwrap().unwrap();
        assert_eq!(banner.content, "New text");
        assert!(banner.active);

        let res = test::call_service(&app, get("/admin/banners/9/edit", &cookie).to_request()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res =
            test::call_service(&app, post("/admin/banners/9/edit", &cookie, &[("content", "x")]).to_request()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let (log, _) = AdminLog::page(&mut pool.get().unwrap(), 1, 10).unwrap();
        let log: Vec<&str> = log.iter().rev().map(|e| e.entry.log.as_str()).collect();
        assert_eq!(log, ["Created banner", "Edited banner #1"]);
    }
}
