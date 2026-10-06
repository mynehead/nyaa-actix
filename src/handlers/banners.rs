//! Admin > Banners: site-wide notices shown above the torrent list on the main page.

use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::Connection;
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::{AdminLog, Banner, User};
use crate::utils::context::base_context;
use crate::utils::{flash, internal_error};

const MAX_LENGTH: usize = 1024;

fn require_moderator(session: &Session, pool: &DbPool) -> Result<User> {
    let user = get_current_user(session, pool).ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    if !user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok(user)
}

fn back_to_list() -> HttpResponse {
    HttpResponse::SeeOther().insert_header(("Location", "/admin/banners")).finish()
}

pub async fn list(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let user = require_moderator(&session, &pool)?;
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

pub async fn create(session: Session, pool: web::Data<DbPool>, form: web::Form<BannerForm>) -> Result<HttpResponse> {
    let user = require_moderator(&session, &pool)?;
    let content = form.content.trim();
    if content.is_empty() {
        flash::push(&session, "danger", "Banner not saved!", "The content is empty.");
        return Ok(back_to_list());
    }
    if content.chars().count() > MAX_LENGTH {
        flash::push(
            &session,
            "danger",
            "Banner not saved!",
            &format!("The content is longer than {} characters.", MAX_LENGTH),
        );
        return Ok(back_to_list());
    }
    let mut conn = pool.get().map_err(internal_error)?;
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        Banner::create(conn, content, user.id)?;
        AdminLog::add(conn, user.id, "Created banner")
    })
    .map_err(internal_error)?;
    flash::push(&session, "success", "Banner created.", "");
    Ok(back_to_list())
}

pub async fn toggle(session: Session, pool: web::Data<DbPool>, path: web::Path<i32>) -> Result<HttpResponse> {
    let user = require_moderator(&session, &pool)?;
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

pub async fn delete(session: Session, pool: web::Data<DbPool>, path: web::Path<i32>) -> Result<HttpResponse> {
    let user = require_moderator(&session, &pool)?;
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
            maintenance_mode: false,
            site_url: String::new(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            trusted: Default::default(),
        }
    }

    async fn login(session: Session, path: web::Path<i32>) -> HttpResponse {
        crate::middleware::auth::login_user(&session, path.into_inner()).unwrap();
        HttpResponse::Ok().finish()
    }

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
}
