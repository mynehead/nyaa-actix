use crate::config::Config;
use crate::db::schema::{bans, users};
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::{hide_ips, AdminLog, Ban, User, UserStatus};
use crate::utils::context::base_context;
use crate::utils::flash;
use crate::utils::pagination::Pagination;
use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

pub async fn reports(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user =
        get_current_user(&session, &pool).ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    if !current_user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let ctx = base_context(&cfg, Some(&current_user));
    let html = tmpl
        .render("admin/reports.html", &ctx)
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
    let user = get_current_user(session, pool).ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
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
    let (mut logs, total) =
        AdminLog::page(&mut conn, query.page(), ADMIN_PER_PAGE).map_err(actix_web::error::ErrorInternalServerError)?;
    if !current_user.is_superadmin() {
        for entry in &mut logs {
            entry.entry.log = hide_ips(&entry.entry.log);
        }
    }
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("logs", &logs);
    ctx.insert("pagination", &Pagination::new(query.page(), total, ADMIN_PER_PAGE));
    let html = tmpl.render("admin/log.html", &ctx).map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// /admin/bans: every ban, newest first, each with an Unban button.
pub async fn bans(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<PageParams>,
) -> Result<HttpResponse> {
    let current_user = require_moderator(&session, &pool)?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let (bans, total) =
        Ban::page(&mut conn, query.page(), ADMIN_PER_PAGE).map_err(actix_web::error::ErrorInternalServerError)?;
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("bans", &bans);
    ctx.insert("pagination", &Pagination::new(query.page(), total, ADMIN_PER_PAGE));
    ctx.insert("flash_messages", &flash::take(&session));
    let html = tmpl.render("admin/bans.html", &ctx).map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// The Unban button: its value is the ban id.
#[derive(Debug, Deserialize)]
pub struct UnbanForm {
    submit: i32,
}

/// Lifts one ban, reactivates its user and logs it, as upstream's `view_adminbans` POST.
pub async fn bans_post(session: Session, pool: web::Data<DbPool>, form: web::Form<UnbanForm>) -> Result<HttpResponse> {
    let current_user = require_moderator(&session, &pool)?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let ban = Ban::by_id(&mut conn, form.submit)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Ban not found"))?;
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        let mut log = format!("Unbanned ban #{}", ban.id);
        if let Some(user) = ban.user_id.map(|id| User::by_id(conn, id)).transpose()?.flatten() {
            log.push(' ');
            log.push_str(&user.username);
            diesel::update(users::table.find(user.id))
                .set(users::status.eq(UserStatus::Active as i32))
                .execute(conn)?;
        }
        if let Some(ip) = ban.ip_string() {
            log.push_str(&format!(" IP({})", ip));
        }
        AdminLog::add(conn, current_user.id, &log)?;
        diesel::delete(bans::table.find(ban.id)).execute(conn)?;
        Ok(())
    })
    .map_err(actix_web::error::ErrorInternalServerError)?;
    flash::push(&session, "success", "", &format!("Unbanned ban #{}", ban.id));
    Ok(HttpResponse::SeeOther().insert_header(("Location", "/admin/bans")).finish())
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
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
                           (1, 'regular', 'x', 1, 0), (2, 'mod', 'x', 1, 2), (3, 'boss', 'x', 1, 3)",
        )
        .execute(&mut conn)
        .unwrap();
        // 10.0.0.9, packed as crate::utils::pack_ip does
        diesel::sql_query("UPDATE users SET last_login_ip = X'0000000000000000000000000A000009' WHERE id = 1")
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
            meili: None,
            trusted: Default::default(),
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
                    .route("/login/{id}", web::get().to(login))
                    .wrap(actix_web::middleware::from_fn(crate::middleware::ip_ban::reject_banned_ip))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/admin/log", web::get().to(log))
                    .route("/admin/bans", web::get().to(bans))
                    .route("/admin/bans", web::post().to(bans_post))
                    .route("/user/{username}", web::get().to(crate::handlers::users::view_user))
                    .route("/user/{username}", web::post().to(crate::handlers::users::ban_user_post)),
            )
            .await;
            let res =
                test::call_service(&app, test::TestRequest::get().uri(&format!("/login/{}", $user)).to_request()).await;
            let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
            (app, cookie)
        }};
    }

    /// GETs `uri` as the cookie's user; returns the status and body.
    macro_rules! page {
        ($app:expr, $cookie:expr, $uri:expr) => {{
            let res =
                test::call_service(&$app, test::TestRequest::get().uri($uri).cookie($cookie.clone()).to_request())
                    .await;
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

    /// POSTs a form as the cookie's user; returns the response.
    macro_rules! post {
        ($app:expr, $cookie:expr, $uri:expr, $form:expr) => {
            test::call_service(
                &$app,
                test::TestRequest::post().uri($uri).cookie($cookie.clone()).set_form($form).to_request(),
            )
            .await
        };
    }

    fn user_status(pool: &DbPool, id: i32) -> i32 {
        users::table.find(id).select(users::status).first(&mut pool.get().unwrap()).unwrap()
    }

    fn all_bans(pool: &DbPool) -> Vec<Ban> {
        bans::table.select(Ban::as_select()).load(&mut pool.get().unwrap()).unwrap()
    }

    fn logs(pool: &DbPool) -> Vec<String> {
        use crate::db::schema::adminlog;
        adminlog::table.order(adminlog::id).select(adminlog::log).load(&mut pool.get().unwrap()).unwrap()
    }

    #[actix_web::test]
    async fn moderators_ban_from_the_user_page_and_unban_from_the_bans_page() {
        let pool = pool();
        let (app, cookie) = app!(pool, 2);
        let (_, html) = page!(app, cookie, "/user/regular");
        assert!(html.contains("Danger Zone") && html.contains("<strong>not banned</strong>"), "{html}");
        assert!(!html.contains("Last login IP"), "only superadmins see IPs");

        // A reason is required
        post!(app, cookie, "/user/regular", &[("reason", " "), ("ban_userip", "Ban User+IP")]);
        assert!(all_bans(&pool).is_empty());

        let res = post!(app, cookie, "/user/regular", &[("reason", "spam"), ("ban_userip", "Ban User+IP")]);
        assert_eq!(res.headers().get("Location").unwrap(), "/user/regular");
        assert_eq!(user_status(&pool, 1), UserStatus::Banned as i32);
        let ban = all_bans(&pool).pop().unwrap();
        assert_eq!((ban.admin_id, ban.user_id, ban.reason.as_str()), (2, Some(1), "spam"));
        assert_eq!(ban.ip_string().as_deref(), Some("10.0.0.9"));
        assert_eq!(logs(&pool), ["User [regular](/user/regular) IP(10.0.0.9) has been banned."]);

        let (_, html) = page!(app, cookie, "/user/regular");
        assert!(html.contains("<strong>ip banned</strong>") && html.contains("name=\"unban\""), "{html}");
        assert!(!html.contains("name=\"ban_user\""), "nothing left to ban, so the ban form is gone");

        let (status, html) = page!(app, cookie, "/admin/bans");
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("<a href=\"/user/regular\">regular</a>") && html.contains("<td>hidden</td>"), "{html}");
        assert!(html.contains(&format!("name=\"submit\" type=\"submit\" value=\"{}\"", ban.id)), "{html}");
        let (app3, cookie3) = app!(pool, 3);
        let (_, html) = page!(app3, cookie3, "/admin/bans");
        assert!(html.contains("<td>10.0.0.9</td>"), "superadmins see IPs");

        let id = ban.id.to_string();
        let res = post!(app, cookie, "/admin/bans", &[("submit", id.as_str())]);
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(all_bans(&pool).is_empty());
        assert_eq!(user_status(&pool, 1), UserStatus::Active as i32);
        assert_eq!(logs(&pool)[1], format!("Unbanned ban #{} regular IP(10.0.0.9)", ban.id));
        // The flash rides in the session cookie the POST set
        let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
        let (_, html) = page!(app, cookie, "/admin/bans");
        assert!(html.contains(&format!("Unbanned ban #{}", ban.id)), "flash shows");
    }

    #[actix_web::test]
    async fn user_page_unban_lifts_every_ban_and_logs_it() {
        let pool = pool();
        let (app, cookie) = app!(pool, 3);
        post!(app, cookie, "/user/regular", &[("reason", "a"), ("ban_user", "Ban User")]);
        post!(app, cookie, "/user/regular", &[("reason", "b"), ("ban_userip", "Ban User+IP")]);
        assert_eq!(all_bans(&pool).len(), 2);
        // Already banned, so a second plain ban does nothing
        post!(app, cookie, "/user/regular", &[("reason", "c"), ("ban_user", "Ban User")]);
        assert_eq!(all_bans(&pool).len(), 2);

        let (_, html) = page!(app, cookie, "/user/regular");
        assert!(html.contains("<dd>10.0.0.9</dd>"), "superadmins see the last login IP");

        post!(app, cookie, "/user/regular", &[("unban", "Unban")]);
        assert!(all_bans(&pool).is_empty());
        assert_eq!(user_status(&pool, 1), UserStatus::Active as i32);
        assert_eq!(logs(&pool).last().unwrap(), "User [regular](/user/regular) IP(10.0.0.9) has been unbanned.");
    }

    #[actix_web::test]
    async fn only_higher_ranked_moderators_can_ban() {
        let pool = pool();
        let (app, cookie) = app!(pool, 2);
        let res = post!(app, cookie, "/user/boss", &[("reason", "x"), ("ban_user", "Ban User")]);
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let (_, html) = page!(app, cookie, "/user/boss");
        assert!(!html.contains("Danger Zone"));

        let (app, cookie) = app!(pool, 1);
        assert_eq!(page!(app, cookie, "/admin/bans").0, StatusCode::FORBIDDEN);
        let res = post!(app, cookie, "/user/mod", &[("reason", "x"), ("ban_user", "Ban User")]);
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(all_bans(&pool).is_empty());
    }

    #[actix_web::test]
    async fn posts_from_banned_ips_are_refused() {
        let pool = pool();
        let (app, cookie) = app!(pool, 3);
        post!(app, cookie, "/user/regular", &[("reason", "x"), ("ban_userip", "Ban User+IP")]);

        let banned: std::net::SocketAddr = "10.0.0.9:1234".parse().unwrap();
        let res = test::call_service(
            &app,
            test::TestRequest::post().uri("/admin/bans").peer_addr(banned).set_form([("submit", "1")]).to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(test::read_body(res).await, "You are banned.");
        // GETs still work, and other IPs can still post
        let res =
            test::call_service(&app, test::TestRequest::get().uri("/user/regular").peer_addr(banned).to_request())
                .await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/user/regular")
                .peer_addr("10.0.0.8:1".parse().unwrap())
                .cookie(cookie.clone())
                .set_form([("unban", "Unban")])
                .to_request(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(all_bans(&pool).is_empty());
    }
}
