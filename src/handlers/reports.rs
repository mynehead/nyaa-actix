//! Reporting torrents and groups (upstream `torrents.submit_report`) and the moderators'
//! queue at /admin/reports (upstream `admin.reports`).

use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::schema::nyaa_torrents;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::{
    validate_reason, Group, GroupReport, Report, Torrent, TorrentFlags, User,
    REPORTS_PER_PAGE, REPORT_INVALID, REPORT_VALID,
};
use crate::utils::context::base_context;
use crate::utils::flash;
use crate::utils::pagination::Pagination;

/// Whether `user` may see the Report button and send reports: logged in, and the account is
/// older than RATELIMIT_ACCOUNT_AGE (upstream `g.user.age > config['RATELIMIT_ACCOUNT_AGE']`).
pub fn can_report(user: Option<&User>, cfg: &Config) -> bool {
    user.is_some_and(|u| u.age_secs() > cfg.ratelimit_account_age)
}

#[derive(Debug, Deserialize)]
pub struct ReportForm {
    #[serde(default)]
    pub reason: String,
}

pub async fn submit_torrent_report(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
    form: web::Form<ReportForm>,
) -> Result<HttpResponse> {
    let user = get_current_user(&session, &pool);
    if !can_report(user.as_ref(), &cfg) {
        return Err(actix_web::error::ErrorForbidden("You may not report torrents"));
    }
    let user = user.expect("can_report requires a user");
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let torrent = Torrent::by_id(&mut conn, path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    // Deleted torrents are 404 to everyone but moderators, and banned ones need no report
    if torrent.is_banned() || (torrent.is_deleted() && !user.is_moderator()) {
        return Err(actix_web::error::ErrorNotFound("Torrent not found"));
    }

    match validate_reason(&form.reason) {
        Ok(reason) => {
            Report::create(&mut conn, torrent.id, user.id, &reason)
                .map_err(actix_web::error::ErrorInternalServerError)?;
            flash::push(&session, "success", "", "Successfully reported torrent!");
        }
        Err(msg) => flash::push(&session, "danger", "", msg),
    }
    Ok(redirect(&format!("/view/{}", torrent.id)))
}

pub async fn submit_group_report(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    form: web::Form<ReportForm>,
) -> Result<HttpResponse> {
    let user = get_current_user(&session, &pool);
    if !can_report(user.as_ref(), &cfg) {
        return Err(actix_web::error::ErrorForbidden("You may not report groups"));
    }
    let user = user.expect("can_report requires a user");
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let group = Group::by_slug(&mut conn, &path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Group not found"))?;

    match validate_reason(&form.reason) {
        Ok(reason) => {
            GroupReport::create(&mut conn, group.id, user.id, &reason)
                .map_err(actix_web::error::ErrorInternalServerError)?;
            flash::push(&session, "success", "", "Successfully reported group!");
        }
        Err(msg) => flash::push(&session, "danger", "", msg),
    }
    Ok(redirect(&format!("/group/{}", urlencoding::encode(&group.slug))))
}

#[derive(Debug, Deserialize)]
pub struct ReportsQuery {
    /// Page of torrent reports
    pub p: Option<i64>,
    /// Page of group reports
    pub gp: Option<i64>,
}

fn require_moderator(session: &Session, pool: &DbPool) -> Result<User> {
    let user = get_current_user(session, pool)
        .ok_or_else(|| actix_web::error::ErrorForbidden("Not allowed"))?;
    if !user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok(user)
}

pub async fn admin_reports(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<ReportsQuery>,
) -> Result<HttpResponse> {
    let moderator = require_moderator(&session, &pool)?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let err = actix_web::error::ErrorInternalServerError;

    let (reports, total) = Report::not_reviewed(&mut conn, query.p.unwrap_or(1)).map_err(err)?;
    let pagination = Pagination::new(query.p.unwrap_or(1), total, REPORTS_PER_PAGE);
    let mut rows = Vec::with_capacity(reports.len());
    for report in reports {
        let Some(torrent) = Torrent::by_id(&mut conn, report.torrent_id).map_err(err)? else { continue };
        let reporter = match report.user_id {
            Some(uid) => User::by_id(&mut conn, uid).map_err(err)?,
            None => None,
        };
        let uploader = match torrent.uploader_id {
            Some(uid) => User::by_id(&mut conn, uid).map_err(err)?,
            None => None,
        };
        // Upstream shows the uploader's IP to superadmins only
        let uploader_ip = torrent.uploader_ip.as_deref()
            .filter(|_| moderator.is_superadmin())
            .and_then(crate::utils::ip_string);
        rows.push(serde_json::json!({
            "report": report, "torrent": torrent, "reporter": reporter,
            "uploader": uploader, "uploader_ip": uploader_ip,
        }));
    }

    let (group_reports, group_total) = GroupReport::not_reviewed(&mut conn, query.gp.unwrap_or(1)).map_err(err)?;
    let group_pagination = Pagination::new(query.gp.unwrap_or(1), group_total, REPORTS_PER_PAGE);
    let mut group_rows = Vec::with_capacity(group_reports.len());
    for report in group_reports {
        let Some(group) = Group::by_id(&mut conn, report.group_id).map_err(err)? else { continue };
        let reporter = match report.user_id {
            Some(uid) => User::by_id(&mut conn, uid).map_err(err)?,
            None => None,
        };
        group_rows.push(serde_json::json!({ "report": report, "group": group, "reporter": reporter }));
    }

    let mut ctx = base_context(&cfg, Some(&moderator));
    ctx.insert("reports", &rows);
    ctx.insert("pagination", &pagination);
    ctx.insert("group_reports", &group_rows);
    ctx.insert("group_pagination", &group_pagination);
    ctx.insert("flash_messages", &flash::take(&session));
    let html = tmpl.render("reports.html", &ctx).map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[derive(Debug, Deserialize)]
pub struct ReportActionForm {
    /// close, hide or delete (upstream ReportActionForm); group reports only close
    pub action: String,
    /// A torrent report's id
    pub report: Option<i32>,
    /// A group report's id
    pub group_report: Option<i32>,
}

pub async fn admin_reports_post(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    form: web::Form<ReportActionForm>,
) -> Result<HttpResponse> {
    let moderator = require_moderator(&session, &pool)?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let err = actix_web::error::ErrorInternalServerError;
    let not_found = || actix_web::error::ErrorNotFound("Report not found");

    if let Some(id) = form.group_report {
        let report = GroupReport::by_id(&mut conn, id).map_err(err)?
            .filter(|r| r.status == crate::models::REPORT_IN_REVIEW)
            .ok_or_else(not_found)?;
        let group = Group::by_id(&mut conn, report.group_id).map_err(err)?.ok_or_else(not_found)?;
        if form.action != "close" {
            return Err(actix_web::error::ErrorBadRequest("Unknown action"));
        }
        GroupReport::review_all(&mut conn, group.id, REPORT_INVALID).map_err(err)?;
        // TODO(admin log): write this to the admin log once its table and helper are on master
        log::info!("Group report #{}: Closed [{}](/group/{}), by {}", report.id, group.name, group.slug, moderator.username);
        flash::push(&session, "success", "", &format!("Closed group report #{}", report.id));
        return Ok(redirect("/admin/reports"));
    }

    let report = Report::by_id(&mut conn, form.report.ok_or_else(not_found)?).map_err(err)?
        .filter(|r| r.status == crate::models::REPORT_IN_REVIEW)
        .ok_or_else(not_found)?;
    let torrent = Torrent::by_id(&mut conn, report.torrent_id).map_err(err)?.ok_or_else(not_found)?;
    let (verb, flag, status) = match form.action.as_str() {
        "delete" => ("Deleted", Some(TorrentFlags::DELETED), REPORT_VALID),
        "hide" => ("Hid", Some(TorrentFlags::HIDDEN), REPORT_VALID),
        "close" => ("Closed", None, REPORT_INVALID),
        _ => return Err(actix_web::error::ErrorBadRequest("Unknown action")),
    };
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        if let Some(flag) = flag {
            diesel::update(nyaa_torrents::table.find(torrent.id))
                .set(nyaa_torrents::flags.eq(torrent.flags | flag.bits()))
                .execute(conn)?;
        }
        Report::review_all(conn, torrent.id, status)?;
        Ok(())
    }).map_err(err)?;
    if flag.is_some() {
        crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), torrent.id);
    }

    // Upstream's admin log entry for the action
    let reporter = match report.user_id {
        Some(uid) => User::by_id(&mut conn, uid).map_err(err)?,
        None => None,
    };
    let reporter = reporter.map_or_else(|| "[deleted user]".to_string(),
        |u| format!("[{}](/user/{})", u.username, urlencoding::encode(&u.username)));
    let entry = format!("Report #{}: {} [#{}](/view/{}), reported by {}",
        report.id, verb, torrent.id, torrent.id, reporter);
    // TODO(admin log): write `entry` to the admin log once its table and helper are on master
    log::info!("{entry} (by {})", moderator.username);

    flash::push(&session, "success", "", &format!("Closed report #{}", report.id));
    Ok(redirect("/admin/reports"))
}

fn redirect(location: &str) -> HttpResponse {
    HttpResponse::Found().insert_header(("Location", location)).finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::REPORT_IN_REVIEW;
    use crate::storage::Storage;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{cookie::{Cookie, Key}, http::StatusCode, test, App};
    use diesel::r2d2::Pool;

    fn pool() -> DbPool {
        // One connection, so every request sees the same in-memory database
        let pool = Pool::builder().max_size(1)
            .build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        // User 4 signed up just now; the others a month ago
        diesel::sql_query("INSERT INTO users (id, username, password_hash, status, level, created_time) VALUES \
                           (1, 'owner', 'x', 1, 1, '2026-01-01 00:00:00'), (2, 'reporter', 'x', 1, 0, '2026-01-01 00:00:00'), \
                           (3, 'mod', 'x', 1, 2, '2026-01-01 00:00:00'), (4, 'newbie', 'x', 1, 0, CURRENT_TIMESTAMP)")
            .execute(&mut conn).unwrap();
        diesel::sql_query(format!(
            "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, information, description, \
             flags, uploader_id, uploader_ip, main_category_id, sub_category_id) \
             VALUES (5, X'{}', 'Reported torrent', 'r.torrent', '', '', 0, 1, X'0000000000000000000000007f000001', 1, 2)",
            "ab".repeat(20)
        )).execute(&mut conn).unwrap();
        diesel::sql_query("INSERT INTO groups (id, name, tag, slug, owner_id) VALUES (7, 'Some Group', 'SG', 'some-group', 1)")
            .execute(&mut conn).unwrap();
        pool
    }

    fn config(account_age: i64) -> Config {
        let storage = std::env::temp_dir().join(format!("nyaa-report-test-{}", std::process::id()));
        Config {
            database_url: String::new(), secret_key: String::new(), site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(), results_per_page: 75, max_pages: 0,
            torrent_storage_path: storage.to_string_lossy().into_owned(), avatar_storage_path: String::new(),
            enable_gravatar: false, maintenance_mode: false, site_url: String::new(), tracker_urls: vec![],
            ratelimit_account_age: account_age, meili: None,
        }
    }

    async fn login(session: Session, path: web::Path<i32>) -> HttpResponse {
        crate::middleware::auth::login_user(&session, path.into_inner()).unwrap();
        HttpResponse::Ok().finish()
    }

    /// The report routes plus the pages showing the button; returns the app and a session cookie.
    macro_rules! app {
        ($pool:expr, $user:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            let dir = config(0).torrent_storage_path;
            let app = test::init_service(App::new()
                .app_data(web::Data::new(config(24 * 3600)))
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(Storage::local(&dir, &dir).unwrap()))
                .app_data(web::Data::new(tera))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/login/{id}", web::get().to(login))
                .route("/view/{id}", web::get().to(crate::handlers::torrents::view_torrent))
                .route("/view/{id}/submit_report", web::post().to(submit_torrent_report))
                .route("/group/{slug}", web::get().to(crate::handlers::groups::view_group))
                .route("/group/{slug}/submit_report", web::post().to(submit_group_report))
                .route("/admin/reports", web::get().to(admin_reports))
                .route("/admin/reports", web::post().to(admin_reports_post))).await;
            let cookie: Option<Cookie<'static>> = match $user {
                Some(id) => {
                    let res = test::call_service(&app, test::TestRequest::get().uri(&format!("/login/{}", id)).to_request()).await;
                    res.response().cookies().next().map(|c| c.into_owned())
                }
                None => None,
            };
            (app, cookie)
        }};
    }

    fn get(uri: &str, cookie: &Option<Cookie<'static>>) -> test::TestRequest {
        let mut req = test::TestRequest::get().uri(uri);
        if let Some(c) = cookie { req = req.cookie(c.clone()); }
        req
    }

    fn post(uri: &str, cookie: &Option<Cookie<'static>>, form: &[(&str, &str)]) -> test::TestRequest {
        let mut req = test::TestRequest::post().uri(uri).set_form(form);
        if let Some(c) = cookie { req = req.cookie(c.clone()); }
        req
    }

    /// Sends `req` and returns the status, keeping the session cookie the response set
    /// (it carries the flash message to the next page).
    macro_rules! send {
        ($app:expr, $req:expr, $cookie:expr) => {{
            let res = test::call_service(&$app, $req.to_request()).await;
            if let Some(c) = res.response().cookies().next() { *$cookie = Some(c.into_owned()); }
            res.status()
        }};
    }

    macro_rules! page {
        ($app:expr, $uri:expr, $cookie:expr) => {{
            let res = test::call_service(&$app, get($uri, $cookie).to_request()).await;
            if let Some(c) = res.response().cookies().next() { *$cookie = Some(c.into_owned()); }
            String::from_utf8(test::read_body(res).await.to_vec()).unwrap()
        }};
    }

    fn statuses(pool: &DbPool) -> Vec<i32> {
        crate::db::schema::nyaa_reports::table.select(crate::db::schema::nyaa_reports::status)
            .order(crate::db::schema::nyaa_reports::id).load(&mut pool.get().unwrap()).unwrap()
    }

    fn flags(pool: &DbPool) -> i32 {
        Torrent::by_id(&mut pool.get().unwrap(), 5).unwrap().unwrap().flags
    }

    #[actix_web::test]
    async fn report_button_needs_an_account_older_than_the_limit() {
        let pool = pool();
        for (user, button) in [(None, false), (Some(4), false), (Some(2), true)] {
            let (app, mut cookie) = app!(pool, user);
            let view = page!(app, "/view/5", &mut cookie);
            assert_eq!(view.contains("data-target=\"#reportModal\""), button, "{user:?}");
            assert_eq!(view.contains("action=\"/view/5/submit_report\""), button, "{user:?}: {view}");
            let group = page!(app, "/group/some-group", &mut cookie);
            assert_eq!(group.contains("action=\"/group/some-group/submit_report\""), button, "{user:?}: {group}");
            if !button {
                let status = send!(app, post("/view/5/submit_report", &cookie, &[("reason", "spam")]), &mut cookie);
                assert_eq!(status, StatusCode::FORBIDDEN, "{user:?}");
            }
        }
        assert!(statuses(&pool).is_empty());
    }

    #[actix_web::test]
    async fn reports_are_saved_with_a_flash() {
        let pool = pool();
        let (app, mut cookie) = app!(pool, Some(2));
        let status = send!(app, post("/view/5/submit_report", &cookie, &[("reason", "ab")]), &mut cookie);
        assert_eq!(status, StatusCode::FOUND);
        assert!(page!(app, "/view/5", &mut cookie).contains("Report reason must be at least 3 characters long"));
        assert!(statuses(&pool).is_empty());

        send!(app, post("/view/5/submit_report", &cookie, &[("reason", " Fake release ")]), &mut cookie);
        let view = page!(app, "/view/5", &mut cookie);
        assert!(view.contains("Successfully reported torrent!"), "{view}");
        // Shown once
        assert!(!page!(app, "/view/5", &mut cookie).contains("Successfully reported torrent!"));
        assert_eq!(statuses(&pool), vec![REPORT_IN_REVIEW]);

        send!(app, post("/group/some-group/submit_report", &cookie, &[("reason", "Impersonation")]), &mut cookie);
        assert!(page!(app, "/group/some-group", &mut cookie).contains("Successfully reported group!"));
        let reports = GroupReport::not_reviewed(&mut pool.get().unwrap(), 1).unwrap();
        assert_eq!((reports.0[0].reason.as_str(), reports.1), ("Impersonation", 1));

        let status = send!(app, post("/view/99/submit_report", &cookie, &[("reason", "gone")]), &mut cookie);
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn queue_is_for_moderators_only() {
        let pool = pool();
        Report::create(&mut pool.get().unwrap(), 5, 2, "Fake release").unwrap();
        for user in [None, Some(1), Some(2)] {
            let (app, mut cookie) = app!(pool, user);
            let res = test::call_service(&app, get("/admin/reports", &cookie).to_request()).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{user:?}");
            let status = send!(app, post("/admin/reports", &cookie, &[("report", "1"), ("action", "delete")]), &mut cookie);
            assert_eq!(status, StatusCode::FORBIDDEN, "{user:?}");
        }
        assert_eq!((statuses(&pool), flags(&pool)), (vec![REPORT_IN_REVIEW], 0));
    }

    #[actix_web::test]
    async fn queue_lists_open_reports() {
        let pool = pool();
        Report::create(&mut pool.get().unwrap(), 5, 2, "Fake release").unwrap();
        let (app, mut cookie) = app!(pool, Some(3));
        let html = page!(app, "/admin/reports", &mut cookie);
        assert!(html.contains("<a href=\"/user/reporter\">reporter</a>"), "{html}");
        assert!(html.contains("<a href=\"/view/5\">Reported torrent</a>"), "{html}");
        assert!(html.contains("<td>Fake release</td>"), "{html}");
        assert!(html.contains("name=\"report\" type=\"hidden\" value=\"1\""), "{html}");
        // The uploader is trusted; their IP is for superadmins only
        assert!(html.contains("Trusted"), "{html}");
        assert!(!html.contains("127.0.0.1"), "{html}");
        assert!(html.contains("No group reports."), "{html}");

        diesel::sql_query("UPDATE users SET level = 3 WHERE id = 3").execute(&mut pool.get().unwrap()).unwrap();
        assert!(page!(app, "/admin/reports", &mut cookie).contains("(127.0.0.1)"));
    }

    #[actix_web::test]
    async fn review_actions_flag_the_torrent_and_close_its_reports() {
        for (action, flag, status) in [("close", 0, REPORT_INVALID),
                                       ("hide", TorrentFlags::HIDDEN.bits(), REPORT_VALID),
                                       ("delete", TorrentFlags::DELETED.bits(), REPORT_VALID)] {
            let pool = pool();
            for reason in ["one", "two"] {
                Report::create(&mut pool.get().unwrap(), 5, 2, reason).unwrap();
            }
            let (app, mut cookie) = app!(pool, Some(3));
            let res = send!(app, post("/admin/reports", &cookie, &[("report", "1"), ("action", action)]), &mut cookie);
            assert_eq!(res, StatusCode::FOUND, "{action}");
            assert_eq!(flags(&pool), flag, "{action}");
            assert_eq!(statuses(&pool), vec![status, status], "{action}");
            let html = page!(app, "/admin/reports", &mut cookie);
            assert!(html.contains("Closed report #1") && html.contains("No torrent reports."), "{action}: {html}");

            // Already reviewed
            let res = send!(app, post("/admin/reports", &cookie, &[("report", "1"), ("action", action)]), &mut cookie);
            assert_eq!(res, StatusCode::NOT_FOUND, "{action}");
        }
    }

    #[actix_web::test]
    async fn group_reports_close() {
        let pool = pool();
        GroupReport::create(&mut pool.get().unwrap(), 7, 2, "Impersonation").unwrap();
        let (app, mut cookie) = app!(pool, Some(3));
        let html = page!(app, "/admin/reports", &mut cookie);
        assert!(html.contains("<a href=\"/group/some-group\">[SG] Some Group</a>"), "{html}");
        let res = send!(app, post("/admin/reports", &cookie, &[("group_report", "1"), ("action", "delete")]), &mut cookie);
        assert_eq!(res, StatusCode::BAD_REQUEST);
        send!(app, post("/admin/reports", &cookie, &[("group_report", "1"), ("action", "close")]), &mut cookie);
        let html = page!(app, "/admin/reports", &mut cookie);
        assert!(html.contains("Closed group report #1") && html.contains("No group reports."), "{html}");
        let report = GroupReport::by_id(&mut pool.get().unwrap(), 1).unwrap().unwrap();
        assert_eq!(report.status, REPORT_INVALID);
    }
}
