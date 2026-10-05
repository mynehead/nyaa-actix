use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use tera::Tera;
use crate::config::Config;
use crate::db::DbPool;
use crate::utils::context::base_context;
use crate::middleware::auth::get_current_user;
use crate::models::{hide_ips, AdminLog, User};
use crate::utils::pagination::Pagination;
use serde::Deserialize;

pub async fn reports(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    if !current_user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let ctx = base_context(&cfg, Some(&current_user));
    let html = tmpl.render("admin/reports.html", &ctx)
        .unwrap_or_else(|_| "<h1>Admin Reports</h1><p>Not yet implemented.</p>".to_string());
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// `?p=N` on the admin lists (upstream also takes `offset`).
#[derive(Debug, Deserialize)]
pub struct PageParams {
    p: Option<i64>,
    offset: Option<i64>,
}

impl PageParams {
    fn page(&self) -> i64 {
        self.p.or(self.offset).unwrap_or(1).max(1)
    }
}

/// Upstream's admin lists show 20 rows a page.
const ADMIN_PER_PAGE: i64 = 20;

/// The signed-in moderator, or 401/403 as upstream's `is_moderator` check.
fn require_moderator(session: &Session, pool: &DbPool) -> Result<User> {
    let user = get_current_user(session, pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    if !user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok(user)
}

/// /admin/log: moderator actions, newest first. Only superadmins see IPs, as upstream.
pub async fn log(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<PageParams>,
) -> Result<HttpResponse> {
    let current_user = require_moderator(&session, &pool)?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let (mut logs, total) = AdminLog::page(&mut conn, query.page(), ADMIN_PER_PAGE)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    if !current_user.is_superadmin() {
        for entry in &mut logs {
            entry.entry.log = hide_ips(&entry.entry.log);
        }
    }
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("logs", &logs);
    ctx.insert("pagination", &Pagination::new(query.page(), total, ADMIN_PER_PAGE));
    let html = tmpl.render("admin/log.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn bans(
    session: Session,
    pool: web::Data<DbPool>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    if !current_user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok(HttpResponse::Ok().body("<h1>Bans</h1><p>Not yet implemented.</p>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{cookie::{Cookie, Key}, http::StatusCode, test, App};
    use diesel::r2d2::Pool;
    use diesel::RunQueryDsl;

    fn pool() -> DbPool {
        let pool = Pool::builder().max_size(1)
            .build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query("INSERT INTO users (id, username, password_hash, status, level) VALUES \
                           (1, 'regular', 'x', 1, 0), (2, 'mod', 'x', 1, 2), (3, 'boss', 'x', 1, 3)")
            .execute(&mut conn).unwrap();
        pool
    }

    fn config() -> Config {
        Config {
            database_url: String::new(), secret_key: String::new(), site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(), results_per_page: 75, max_pages: 0,
            torrent_storage_path: String::new(), avatar_storage_path: String::new(), enable_gravatar: false,
            maintenance_mode: false, site_url: String::new(), tracker_urls: vec![], meili: None,
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
            let app = test::init_service(App::new()
                .app_data(web::Data::new(config()))
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(tera))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/login/{id}", web::get().to(login))
                .route("/admin/log", web::get().to(log))).await;
            let res = test::call_service(&app,
                test::TestRequest::get().uri(&format!("/login/{}", $user)).to_request()).await;
            let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
            (app, cookie)
        }};
    }

    /// GETs `uri` as the cookie's user; returns the status and body.
    macro_rules! page {
        ($app:expr, $cookie:expr, $uri:expr) => {{
            let res = test::call_service(&$app,
                test::TestRequest::get().uri($uri).cookie($cookie.clone()).to_request()).await;
            let status = res.status();
            (status, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
        }};
    }

    #[actix_web::test]
    async fn log_lists_newest_first_and_hides_ips_from_moderators() {
        let pool = pool();
        {
            let mut conn = pool.get().unwrap();
            AdminLog::add(&mut conn, 2, "Torrent [#5](/view/5) has been deleted").unwrap();
            AdminLog::add(&mut conn, 3, "User [x](/user/x) IP(10.1.2.3) has been banned.").unwrap();
        }

        let (app, cookie) = app!(pool, 1);
        assert_eq!(page!(app, cookie, "/admin/log").0, StatusCode::FORBIDDEN);

        let (app, cookie) = app!(pool, 2);
        let (status, html) = page!(app, cookie, "/admin/log");
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("<th>Moderator/Admin</th>"), "{html}");
        assert!(html.contains("IP(hidden)") && !html.contains("10.1.2.3"), "{html}");
        let banned = html.find("has been banned").unwrap();
        let deleted = html.find("has been deleted").unwrap();
        assert!(banned < deleted, "newest first");
        assert!(html.contains("<a href=\"/user/boss\">boss</a>"), "{html}");

        let (app, cookie) = app!(pool, 3);
        let (_, html) = page!(app, cookie, "/admin/log");
        assert!(html.contains("IP(10.1.2.3)"), "superadmins see IPs");
    }

    #[actix_web::test]
    async fn log_pages_by_twenty() {
        let pool = pool();
        {
            let mut conn = pool.get().unwrap();
            for i in 1..=25 {
                AdminLog::add(&mut conn, 2, &format!("entry {i:02}")).unwrap();
            }
        }
        let (app, cookie) = app!(pool, 2);
        let (_, first) = page!(app, cookie, "/admin/log");
        assert!(first.contains("entry 25") && first.contains("entry 06") && !first.contains("entry 05"));
        assert!(first.contains("href=\"/admin/log?p=2\""), "{first}");
        let (_, second) = page!(app, cookie, "/admin/log?p=2");
        assert!(second.contains("entry 05") && second.contains("entry 01") && !second.contains("entry 06"));
    }
}
