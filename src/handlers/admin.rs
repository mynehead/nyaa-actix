use crate::auth::{Moderator, Permission};
use crate::config::Config;
use crate::db::schema::{bans, users};
use crate::db::DbPool;
use crate::middleware::ip_range_ban::IpRangeBans;
use crate::models::{hide_ips, AdminLog, Ban, IpRangeBan, NewIpRangeBan, User, UserStatus, MAX_BAN_REASON_LEN};
use crate::utils::context::base_context;
use crate::utils::pagination::Pagination;
use crate::utils::proxy::IpNet;
use crate::utils::{client_addr, flash, internal_error};
use actix_session::Session;
use actix_web::{web, HttpRequest, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

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

/// /admin/log: moderator actions, newest first. Only superadmins see IPs, as upstream.
pub async fn log(
    Moderator(current_user): Moderator,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<PageParams>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let (mut logs, total) = AdminLog::page(&mut conn, query.page(), ADMIN_PER_PAGE).map_err(internal_error)?;
    if !current_user.can(Permission::SeeIps) {
        for entry in &mut logs {
            entry.entry.log = hide_ips(&entry.entry.log);
        }
    }
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("logs", &logs);
    ctx.insert("pagination", &Pagination::new(query.page(), total, ADMIN_PER_PAGE));
    let html = tmpl.render("admin/log.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// /admin/bans: every ban, newest first, each with an Unban button.
pub async fn bans(
    Moderator(current_user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<PageParams>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let (bans, total) = Ban::page(&mut conn, query.page(), ADMIN_PER_PAGE).map_err(internal_error)?;
    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("bans", &bans);
    if current_user.can(Permission::BanIpRanges) {
        let ranges = IpRangeBan::list(&mut conn, chrono::Utc::now().naive_utc()).map_err(internal_error)?;
        ctx.insert("range_bans", &ranges);
        ctx.insert("range_ban_durations", &RANGE_BAN_DURATIONS);
    }
    ctx.insert("pagination", &Pagination::new(query.page(), total, ADMIN_PER_PAGE));
    ctx.insert("flash_messages", &flash::take(&session));
    let html = tmpl.render("admin/bans.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// The Unban button: its value is the ban id.
#[derive(Debug, Deserialize)]
pub struct UnbanForm {
    submit: i32,
}

/// Lifts one ban, reactivates its user and logs it, as upstream's `view_adminbans` POST.
pub async fn bans_post(
    Moderator(current_user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    form: web::Form<UnbanForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let ban = Ban::by_id(&mut conn, form.submit)
        .map_err(internal_error)?
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
    .map_err(internal_error)?;
    flash::push(&session, "success", "", &format!("Unbanned ban #{}", ban.id));
    Ok(HttpResponse::SeeOther().insert_header(("Location", "/admin/bans")).finish())
}

/// The expiry choices on the range ban form: hours, and the label shown.
const RANGE_BAN_DURATIONS: [(i64, &str); 6] =
    [(1, "1 hour"), (24, "1 day"), (168, "1 week"), (720, "30 days"), (2160, "90 days"), (8760, "1 year")];

#[derive(Debug, Deserialize)]
pub struct RangeBanForm {
    cidr: String,
    #[serde(default)]
    reason: String,
    /// Hours from `RANGE_BAN_DURATIONS`; empty means until lifted.
    #[serde(default)]
    duration: String,
}

/// Why a range can't be banned, or the canonical network to ban.
fn check_range(cidr: &str, own: Option<std::net::IpAddr>) -> std::result::Result<IpNet, &'static str> {
    let net = IpNet::parse(cidr.trim())
        .ok_or("Not an IP address or network, for example 203.0.113.0/24 or 2001:db8::/32.")?
        .network();
    if own.is_some_and(|ip| net.contains(ip)) {
        return Err("That range includes your own address; banning it would lock you out.");
    }
    // Without TRUSTED_PROXIES behind a local reverse proxy every visitor would look like this
    if ["127.0.0.1", "::1"].iter().any(|ip| net.contains(ip.parse().unwrap())) {
        return Err("Loopback addresses can't be banned.");
    }
    Ok(net)
}

fn back_to_bans() -> HttpResponse {
    HttpResponse::SeeOther().insert_header(("Location", "/admin/bans")).finish()
}

/// Bans a network from the whole site and logs it.
pub async fn range_ban_add(
    Moderator(current_user): Moderator,
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    range_bans: web::Data<IpRangeBans>,
    form: web::Form<RangeBanForm>,
) -> Result<HttpResponse> {
    if !current_user.can(Permission::BanIpRanges) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let net = match check_range(&form.cidr, client_addr(&req)) {
        Ok(net) => net,
        Err(msg) => {
            flash::push(&session, "danger", "", msg);
            return Ok(back_to_bans());
        }
    };
    let reason = form.reason.trim();
    if reason.chars().count() > MAX_BAN_REASON_LEN {
        flash::push(&session, "danger", "", "The reason is too long.");
        return Ok(back_to_bans());
    }
    let hours = match form.duration.trim() {
        "" => None,
        h => match RANGE_BAN_DURATIONS.iter().find(|(d, _)| d.to_string() == h) {
            Some((d, _)) => Some(*d),
            None => {
                flash::push(&session, "danger", "", "Pick an expiry from the list.");
                return Ok(back_to_bans());
            }
        },
    };
    let now = chrono::Utc::now().naive_utc();
    let ban = NewIpRangeBan {
        cidr: net.to_string(),
        reason: reason.to_owned(),
        created_time: now,
        expires_time: hours.map(|h| now + chrono::Duration::hours(h)),
        admin_id: current_user.id,
    };
    let mut conn = pool.get().map_err(internal_error)?;
    let added = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            if let Some(old) = IpRangeBan::by_cidr(conn, &ban.cidr)? {
                if !old.is_expired(now) {
                    return Ok(false);
                }
                IpRangeBan::delete(conn, old.id)?;
            }
            IpRangeBan::insert(conn, &ban)?;
            let until = match ban.expires_time {
                Some(t) => format!(" until {} UTC", t.format("%Y-%m-%d %H:%M")),
                None => String::new(),
            };
            let because = if reason.is_empty() { String::new() } else { format!(": {reason}") };
            AdminLog::add(conn, current_user.id, &format!("Banned IP range IP({}){until}{because}", ban.cidr))?;
            Ok(true)
        })
        .map_err(internal_error)?;
    if added {
        range_bans.reload(&mut conn).map_err(internal_error)?;
        flash::push(&session, "success", "", &format!("Banned {}", ban.cidr));
    } else {
        flash::push(&session, "danger", "", &format!("{} is already banned.", ban.cidr));
    }
    Ok(back_to_bans())
}

/// Lifts a range ban and logs it.
pub async fn range_ban_remove(
    Moderator(current_user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    range_bans: web::Data<IpRangeBans>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    if !current_user.can(Permission::BanIpRanges) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let mut conn = pool.get().map_err(internal_error)?;
    let ban = IpRangeBan::by_id(&mut conn, path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Ban not found"))?;
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        IpRangeBan::delete(conn, ban.id)?;
        AdminLog::add(conn, current_user.id, &format!("Lifted IP range ban #{} IP({})", ban.id, ban.cidr))
    })
    .map_err(internal_error)?;
    range_bans.reload(&mut conn).map_err(internal_error)?;
    flash::push(&session, "success", "", &format!("Unbanned {}", ban.cidr));
    Ok(back_to_bans())
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
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
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
                    .app_data(web::Data::new(IpRangeBans::load(&mut $pool.get().unwrap()).unwrap()))
                    .route("/login/{id}", web::get().to(login))
                    .wrap(actix_web::middleware::from_fn(crate::middleware::ip_ban::reject_banned_ip))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .wrap(actix_web::middleware::from_fn(crate::middleware::ip_range_ban::reject_banned_range))
                    .route("/admin/log", web::get().to(log))
                    .route("/admin/bans", web::get().to(bans))
                    .route("/admin/bans", web::post().to(bans_post))
                    .route("/admin/bans/ranges", web::post().to(range_ban_add))
                    .route("/admin/bans/ranges/{id}/delete", web::post().to(range_ban_remove))
                    .route("/user/{username}", web::get().to(crate::handlers::users::view_user))
                    .route("/user/{username}", web::post().to(crate::handlers::users::ban_user_post))
                    .route("/user/{username}/nuke/torrents", web::post().to(crate::handlers::users::nuke_torrents_post))
                    .route(
                        "/user/{username}/nuke/comments",
                        web::post().to(crate::handlers::users::nuke_comments_post),
                    ),
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

    #[actix_web::test]
    async fn superadmins_nuke_torrents_and_comments() {
        use crate::db::schema::{nyaa_comments, nyaa_torrents};
        let pool = pool();
        {
            let mut conn = pool.get().unwrap();
            for (id, uploader) in [(1, 1), (2, 1), (3, 2)] {
                diesel::sql_query(format!(
                    "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, information, description, \
                     flags, uploader_id, main_category_id, sub_category_id, comment_count) \
                     VALUES ({id}, X'{}', 't', 't.torrent', '', '', 0, {uploader}, 1, 2, 2)",
                    format!("{id:02}").repeat(20)
                ))
                .execute(&mut conn)
                .unwrap();
                diesel::sql_query(format!(
                    "INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) \
                                           VALUES ({id}, 4, 3, 9)"
                ))
                .execute(&mut conn)
                .unwrap();
            }
            diesel::sql_query(
                "INSERT INTO nyaa_comments (torrent_id, user_id, text) VALUES \
                               (3, 1, 'a'), (3, 2, 'b'), (2, 1, 'c')",
            )
            .execute(&mut conn)
            .unwrap();
        }

        // Moderators can't nuke
        let (app, cookie) = app!(pool, 2);
        let res = post!(app, cookie, "/user/regular/nuke/torrents", &[("nuke_torrents", "x")]);
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let (_, html) = page!(app, cookie, "/user/regular");
        assert!(!html.contains("Nuke Torrents"));

        let (app, cookie) = app!(pool, 3);
        let (_, html) = page!(app, cookie, "/user/regular");
        assert!(html.contains("formaction=\"/user/regular/nuke/torrents\""), "{html}");

        let res = post!(app, cookie, "/user/regular/nuke/torrents", &[("nuke_torrents", "x")]);
        assert_eq!(res.headers().get("Location").unwrap(), "/user/regular");
        let mut conn = pool.get().unwrap();
        let flags: Vec<(i32, i32)> = nyaa_torrents::table
            .order(nyaa_torrents::id)
            .select((nyaa_torrents::id, nyaa_torrents::flags))
            .load(&mut conn)
            .unwrap();
        let banned = (crate::models::TorrentFlags::DELETED | crate::models::TorrentFlags::BANNED).bits();
        assert_eq!(flags, [(1, banned), (2, banned), (3, 0)]);
        drop(conn);

        let res = post!(app, cookie, "/user/regular/nuke/comments", &[("nuke_comments", "x")]);
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let mut conn = pool.get().unwrap();
        let left: Vec<String> = nyaa_comments::table.select(nyaa_comments::text).load(&mut conn).unwrap();
        assert_eq!(left, ["b"]);
        let counts: Vec<i32> =
            nyaa_torrents::table.order(nyaa_torrents::id).select(nyaa_torrents::comment_count).load(&mut conn).unwrap();
        assert_eq!(counts, [2, 0, 1], "recounted only where comments were removed");
        drop(conn);
        assert_eq!(
            logs(&pool),
            ["Nuked 2 torrents of [regular](/user/regular)", "Nuked 2 comments of [regular](/user/regular)"]
        );
    }

    /// POSTs a form as the cookie's user from `ip`; returns the response.
    macro_rules! post_from {
        ($app:expr, $cookie:expr, $ip:expr, $uri:expr, $form:expr) => {
            test::call_service(
                &$app,
                test::TestRequest::post()
                    .uri($uri)
                    .peer_addr(std::net::SocketAddr::new($ip.parse().unwrap(), 4000))
                    .cookie($cookie.clone())
                    .set_form($form)
                    .to_request(),
            )
            .await
        };
    }

    fn range_bans(pool: &DbPool) -> Vec<IpRangeBan> {
        IpRangeBan::all(&mut pool.get().unwrap()).unwrap()
    }

    /// GETs /admin/bans as a guest from `ip`; returns the status and body.
    macro_rules! get_from {
        ($app:expr, $ip:expr) => {{
            let req = test::TestRequest::get()
                .uri("/admin/bans")
                .peer_addr(std::net::SocketAddr::new($ip.parse().unwrap(), 4000))
                .to_request();
            let res = test::call_service(&$app, req).await;
            let status = res.status();
            (status, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
        }};
    }

    #[actix_web::test]
    async fn superadmins_ban_ip_ranges_from_the_whole_site() {
        let pool = pool();
        let me = "198.51.100.7";

        // Moderators neither see nor use the form
        let (app, cookie) = app!(pool, 2);
        let (_, html) = page!(app, cookie, "/admin/bans");
        assert!(!html.contains("IP range bans"), "{html}");
        let res = post_from!(app, cookie, me, "/admin/bans/ranges", &[("cidr", "10.9.0.0/16")]);
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(range_bans(&pool).is_empty());

        let (app, cookie) = app!(pool, 3);
        let (_, html) = page!(app, cookie, "/admin/bans");
        assert!(html.contains("IP range bans") && html.contains("name=\"cidr\""), "{html}");

        // Refused: garbage, the admin's own address, loopback, an expiry not on the list
        for (cidr, duration) in
            [("nope", ""), ("198.51.0.0/16", ""), ("127.0.0.0/8", ""), ("::/0", ""), ("10.9.0.0/16", "5")]
        {
            post_from!(app, cookie, me, "/admin/bans/ranges", &[("cidr", cidr), ("duration", duration)]);
        }
        assert!(range_bans(&pool).is_empty());

        // Host bits are dropped; IPv6 works the same way
        let res = post_from!(app, cookie, me, "/admin/bans/ranges", &[("cidr", " 10.9.8.7/16 "), ("reason", "botnet")]);
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        post_from!(app, cookie, me, "/admin/bans/ranges", &[("cidr", "2001:db8:1::/48"), ("duration", "24")]);
        // A second ban on the same live range is refused
        post_from!(app, cookie, me, "/admin/bans/ranges", &[("cidr", "10.9.0.0/16")]);
        let bans = range_bans(&pool);
        assert_eq!(bans.len(), 2);
        assert_eq!(
            (bans[0].cidr.as_str(), bans[0].reason.as_str(), bans[0].expires_time),
            ("10.9.0.0/16", "botnet", None)
        );
        assert_eq!(bans[1].cidr, "2001:db8:1::/48");
        assert!(bans[1].expires_time.is_some());
        assert_eq!(logs(&pool)[0], "Banned IP range IP(10.9.0.0/16): botnet");
        assert!(logs(&pool)[1].starts_with("Banned IP range IP(2001:db8:1::/48) until "), "{:?}", logs(&pool));

        // Every request from a banned network is refused, before any login or page code
        let (status, body) = get_from!(app, "10.9.200.1");
        assert_eq!((status, body.as_str()), (StatusCode::FORBIDDEN, "Your network is banned from this site."));
        let (status, body) = get_from!(app, "::ffff:10.9.0.1");
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, body) = get_from!(app, "2001:db8:1:ffff::1");
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains(" until "), "{body}");
        assert_ne!(get_from!(app, "10.10.0.1").0, StatusCode::FORBIDDEN);
        assert_ne!(get_from!(app, "2001:db8:2::1").0, StatusCode::FORBIDDEN);

        let (_, html) = page!(app, cookie, "/admin/bans");
        assert!(html.contains("<code>10.9.0.0&#x2F;16</code>") && html.contains("botnet"), "{html}");

        // Unbanning takes effect at once
        let uri = format!("/admin/bans/ranges/{}/delete", bans[0].id);
        let res = post_from!(app, cookie, me, &uri, &[("x", "")]);
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(range_bans(&pool).len(), 1);
        assert_ne!(get_from!(app, "10.9.200.1").0, StatusCode::FORBIDDEN);
        assert_eq!(logs(&pool)[2], format!("Lifted IP range ban #{} IP(10.9.0.0/16)", bans[0].id));
    }

    #[actix_web::test]
    async fn expired_range_bans_stop_applying() {
        let pool = pool();
        let mut conn = pool.get().unwrap();
        let now = chrono::Utc::now().naive_utc();
        let ban = |cidr: &str, expires| NewIpRangeBan {
            cidr: cidr.into(),
            reason: String::new(),
            created_time: now,
            expires_time: expires,
            admin_id: 3,
        };
        IpRangeBan::insert(&mut conn, &ban("10.1.0.0/16", Some(now - chrono::Duration::hours(1)))).unwrap();
        IpRangeBan::insert(&mut conn, &ban("10.2.0.0/16", Some(now + chrono::Duration::hours(1)))).unwrap();
        let cache = IpRangeBans::load(&mut conn).unwrap();
        assert!(cache.find("10.1.0.1".parse().unwrap(), now).is_none());
        assert!(cache.find("10.2.0.1".parse().unwrap(), now).is_some());
        assert!(cache.find("10.2.0.1".parse().unwrap(), now + chrono::Duration::hours(2)).is_none());
        let listed = IpRangeBan::list(&mut conn, now).unwrap();
        assert_eq!(
            listed.iter().map(|b| (b.ban.cidr.as_str(), b.expired)).collect::<Vec<_>>(),
            [("10.2.0.0/16", false), ("10.1.0.0/16", true)]
        );
    }
}
