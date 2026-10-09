//! Trusted status: the public info page, the application form, and the moderator list
//! and review pages (upstream `site.trusted`, `account.request_trusted`,
//! `admin.trusted` and `admin.trusted_application`).

use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::auth::{CurrentUser, Moderator, Permission};
use crate::config::Config;
use crate::db::DbPool;
use crate::models::{
    trusted_deny_reasons, TrustedApplication, TrustedApplicationStatus, TrustedListFilter, TrustedRecommendation, User,
};
use crate::utils::context::base_context;
use crate::utils::pagination::Pagination;
use crate::utils::{flash, internal_error};

const APPS_PER_PAGE: i64 = 20;

fn render(tmpl: &Tera, name: &str, ctx: &tera::Context) -> Result<String> {
    tmpl.render(name, ctx).map_err(internal_error)
}

fn html(body: String) -> HttpResponse {
    HttpResponse::Ok().content_type("text/html").body(body)
}

fn redirect(to: &str) -> HttpResponse {
    HttpResponse::Found().insert_header(("Location", to)).finish()
}

/// Length check of upstream's `Length(min, max)` validators, counted in characters.
fn length_ok(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.chars().count())
}

/// GET /trusted: the trusted rules and a button to apply.
pub async fn trusted_info(
    CurrentUser(current_user): CurrentUser,
    session: Session,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("active_page", "trusted");
    ctx.insert("flash_messages", &flash::take(&session));
    Ok(html(render(&tmpl, "trusted.html", &ctx)?))
}

#[derive(Debug, Default, Deserialize)]
pub struct TrustedForm {
    #[serde(default)]
    pub why_give_trusted: String,
    #[serde(default)]
    pub why_want_trusted: String,
}

/// GET and POST /trusted/request: the application form, or why the user can't apply.
pub async fn request_trusted(
    CurrentUser(user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: Option<web::Form<TrustedForm>>,
) -> Result<HttpResponse> {
    let Some(user) = user else {
        return Ok(redirect("/login"));
    };
    let mut conn = pool.get().map_err(internal_error)?;
    let deny_reasons = trusted_deny_reasons(&mut conn, &user, &cfg.trusted).map_err(internal_error)?;

    let mut errors = serde_json::Map::new();
    let posted = form.map(|f| f.into_inner());
    if let Some(f) = &posted {
        let why_give = f.why_give_trusted.trim_end();
        let why_want = f.why_want_trusted.trim_end();
        let mut check = |field: &str, value: &str, message: &str| {
            let msg = if value.trim().is_empty() {
                "Please fill out all of the fields in the form."
            } else if !length_ok(value, 32, 4000) {
                message
            } else {
                return;
            };
            errors.insert(field.to_string(), serde_json::json!([msg]));
        };
        check(
            "why_give_trusted",
            why_give,
            "Please explain why you think you should be given trusted status \
               in at least 32 but less than 4000 characters.",
        );
        check(
            "why_want_trusted",
            why_want,
            "Please explain why you want to become a trusted user in at least \
               32 but less than 4000 characters.",
        );
        if errors.is_empty() && deny_reasons.is_empty() {
            TrustedApplication::submit(&mut conn, user.id, why_give, why_want).map_err(internal_error)?;
            flash::push(
                &session,
                "success",
                "",
                "Your trusted application has been submitted. \
                         You will receive an email when a decision has been made.",
            );
            return Ok(redirect("/trusted"));
        }
    }

    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("show_form", &deny_reasons.is_empty());
    ctx.insert("deny_reasons", &deny_reasons);
    ctx.insert("errors", &errors);
    ctx.insert(
        "form",
        &serde_json::json!({
            "why_give_trusted": posted.as_ref().map(|f| f.why_give_trusted.as_str()).unwrap_or(""),
            "why_want_trusted": posted.as_ref().map(|f| f.why_want_trusted.as_str()).unwrap_or(""),
        }),
    );
    Ok(html(render(&tmpl, "trusted_form.html", &ctx)?))
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub p: Option<i64>,
}

/// GET /admin/trusted and /admin/trusted/{new,reviewed,closed}.
pub async fn admin_trusted(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: Option<web::Path<String>>,
    params: web::Query<ListParams>,
) -> Result<HttpResponse> {
    let list_filter = path.map(|p| p.into_inner());
    let filter =
        TrustedListFilter::parse(list_filter.as_deref()).ok_or_else(|| actix_web::error::ErrorNotFound("Not found"))?;

    let mut conn = pool.get().map_err(internal_error)?;
    let page = params.p.unwrap_or(1).max(1);
    let (rows, total) = TrustedApplication::list(&mut conn, filter, page, APPS_PER_PAGE).map_err(internal_error)?;
    let pagination = Pagination::new(page, total, APPS_PER_PAGE);
    let apps: Vec<_> = rows
        .iter()
        .map(|(app, submitter)| {
            serde_json::json!({
                "app": app,
                "submitter": submitter.username,
                "status": TrustedApplicationStatus::name(app.status),
            })
        })
        .collect();

    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("apps", &apps);
    ctx.insert("list_filter", &list_filter.unwrap_or_default());
    ctx.insert("pagination", &pagination);
    Ok(html(render(&tmpl, "admin_trusted.html", &ctx)?))
}

#[derive(Debug, Default, Deserialize)]
pub struct ReviewForm {
    pub comment: Option<String>,
    pub recommendation: Option<String>,
    /// The decision buttons; only superadmins see them.
    pub accept: Option<String>,
    pub reject: Option<String>,
}

/// GET and POST /admin/trusted/application/{id}: the application, its reviews, the review
/// form, and for superadmins the accept and reject buttons.
pub async fn admin_trusted_application(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
    form: Option<web::Form<ReviewForm>>,
) -> Result<HttpResponse> {
    let app_id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;
    let app = TrustedApplication::by_id(&mut conn, app_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Not found"))?;
    let can_decide = user.can(Permission::DecideTrusted) && !app.is_closed();
    let here = format!("/admin/trusted/application/{app_id}");

    let mut comment_errors: Vec<&str> = Vec::new();
    let posted = form.map(|f| f.into_inner()).unwrap_or_default();
    if can_decide && (posted.accept.is_some() || posted.reject.is_some()) {
        let accept = posted.accept.is_some();
        if app.decide(&mut conn, user.id, accept).map_err(internal_error)? {
            // Upstream also emails the submitter; there is no mail yet.
            let verdict = if accept { "accepted" } else { "rejected" };
            flash::push(&session, "success", "", &format!("Application has been {verdict}."));
        }
        return Ok(redirect(&here));
    }
    if let Some(comment) = posted.comment.as_deref().filter(|c| !c.is_empty()) {
        let recommendation = posted.recommendation.as_deref().and_then(TrustedRecommendation::parse);
        if !length_ok(comment, 8, 4000) {
            comment_errors.push("Please provide a comment");
        }
        if let (Some(rec), true) = (recommendation, comment_errors.is_empty()) {
            app.add_review(&mut conn, user.id, comment, rec).map_err(internal_error)?;
            flash::push(&session, "success", "", "Review successfully posted.");
            return Ok(redirect(&here));
        }
    }

    let submitter = User::by_id(&mut conn, app.submitter_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorInternalServerError("Submitter missing"))?;
    let reviews: Vec<_> = app
        .reviews(&mut conn)
        .map_err(internal_error)?
        .into_iter()
        .map(|(rev, reviewer)| {
            serde_json::json!({
                "review": rev,
                "reviewer": reviewer.username,
                "recommendation": TrustedRecommendation::name(rev.recommendation),
            })
        })
        .collect();

    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("app", &app);
    ctx.insert("status", TrustedApplicationStatus::name(app.status));
    ctx.insert("submitter", &submitter.username);
    ctx.insert("reviews", &reviews);
    ctx.insert("can_decide", &can_decide);
    ctx.insert("comment", posted.comment.as_deref().unwrap_or(""));
    ctx.insert("recommendation", posted.recommendation.as_deref().unwrap_or("abstain"));
    ctx.insert("comment_errors", &comment_errors);
    Ok(html(render(&tmpl, "admin_trusted_view.html", &ctx)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TrustedConfig;
    use crate::models::UserLevel;
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
                           (1, 'applicant', 'x', 1, 0), (2, 'mod', 'x', 1, 2), (3, 'admin', 'x', 1, 3)",
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
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            upload_limit: Default::default(),
            meili: None,
            tracker: None,
            trusted: TrustedConfig { min_uploads: 0, min_downloads: 0, reapply_cooldown_days: 90 },
            tickets: Default::default(),
        }
    }

    use crate::middleware::auth::test_support::login;

    /// The trusted routes plus a login shortcut; returns the app and a session cookie for `user`.
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
                    .route("/trusted", web::get().to(trusted_info))
                    .route("/trusted/request", web::get().to(request_trusted))
                    .route("/trusted/request", web::post().to(request_trusted))
                    .route("/admin/trusted", web::get().to(admin_trusted))
                    .route("/admin/trusted/{list_filter}", web::get().to(admin_trusted))
                    .route("/admin/trusted/application/{id}", web::get().to(admin_trusted_application))
                    .route("/admin/trusted/application/{id}", web::post().to(admin_trusted_application)),
            )
            .await;
            let res =
                test::call_service(&app, test::TestRequest::get().uri(&format!("/login/{}", $user)).to_request()).await;
            let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
            (app, cookie)
        }};
    }

    /// Sends with the session cookie and keeps the updated one, as a browser would;
    /// gives the status, Location header and body.
    macro_rules! send {
        ($app:expr, $cookie:expr, $req:expr) => {{
            let res = test::call_service(&$app, $req.cookie($cookie.clone()).to_request()).await;
            if let Some(c) = res.response().cookies().next() {
                $cookie = c.into_owned();
            }
            let status = res.status();
            let location = res.headers().get("Location").map(|l| l.to_str().unwrap().to_string()).unwrap_or_default();
            let body = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
            (status, location, body)
        }};
    }

    fn get(uri: &str) -> test::TestRequest {
        test::TestRequest::get().uri(uri)
    }

    fn post(uri: &str, form: &[(&str, &str)]) -> test::TestRequest {
        test::TestRequest::post().uri(uri).set_form(form)
    }

    const LONG: &str = "I have uploaded plenty of good releases for years.";

    #[actix_web::test]
    #[allow(unused_assignments)] // the last cookie update of each user
    async fn apply_review_and_accept() {
        let pool = pool();
        let (app, mut c) = app!(pool, 1);
        let (_, _, page) = send!(app, c, get("/trusted/request"));
        assert!(page.contains("You are eligible to apply"), "{page}");

        let (status, _, page) =
            send!(app, c, post("/trusted/request", &[("why_give_trusted", "too short"), ("why_want_trusted", "")]));
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("in at least 32 but less than 4000 characters"), "{page}");
        assert!(page.contains("Please fill out all of the fields"), "{page}");
        assert!(page.contains(">too short</textarea>"), "keeps what was typed");

        let (status, location, _) =
            send!(app, c, post("/trusted/request", &[("why_give_trusted", LONG), ("why_want_trusted", LONG)]));
        assert_eq!((status, location.as_str()), (StatusCode::FOUND, "/trusted"));
        let (_, _, page) = send!(app, c, get("/trusted"));
        assert!(page.contains("Your trusted application has been submitted"), "{page}");
        let (_, _, page) = send!(app, c, get("/trusted/request"));
        assert!(page.contains("You already have an open application."), "{page}");

        // Regular users can't see the admin pages
        let (status, _, _) = send!(app, c, get("/admin/trusted"));
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A moderator reviews but cannot decide
        let (app, mut c) = app!(pool, 2);
        let (status, _, page) = send!(app, c, get("/admin/trusted"));
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("List of open applications") && page.contains("/admin/trusted/application/1"), "{page}");
        let (_, _, page) = send!(app, c, get("/admin/trusted/reviewed"));
        assert!(!page.contains("/admin/trusted/application/1"), "{page}");
        let (status, _, _) = send!(app, c, get("/admin/trusted/bogus"));
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, _, page) = send!(app, c, get("/admin/trusted/application/1"));
        assert!(page.contains("applicant's Application") && page.contains(LONG), "{page}");
        assert!(!page.contains("name=\"accept\""), "only superadmins decide");
        let (status, _, _) = send!(app, c, post("/admin/trusted/application/1", &[("accept", "Accept")]));
        assert_eq!(status, StatusCode::OK, "ignored, as upstream ignores the missing decision form");
        let (_, _, page) =
            send!(app, c, post("/admin/trusted/application/1", &[("comment", "short"), ("recommendation", "accept")]));
        assert!(page.contains("Please provide a comment"), "{page}");
        let (_, location, _) = send!(
            app,
            c,
            post(
                "/admin/trusted/application/1",
                &[("comment", "Good uploader, accept."), ("recommendation", "accept")]
            )
        );
        assert_eq!(location, "/admin/trusted/application/1");
        let (_, _, page) = send!(app, c, get("/admin/trusted/application/1"));
        assert!(page.contains("Review successfully posted.") && page.contains("Reviews - 1"), "{page}");
        assert!(page.contains("mod recommends to <strong>accept</strong> this application."), "{page}");
        assert!(page.contains("<dd>Reviewed</dd>"), "{page}");
        assert!(User::by_id(&mut pool.get().unwrap(), 1).unwrap().unwrap().level() < UserLevel::Trusted);

        // The superadmin accepts
        let (app, mut c) = app!(pool, 3);
        let (_, _, page) = send!(app, c, get("/admin/trusted/application/1"));
        assert!(page.contains("name=\"accept\""), "{page}");
        send!(app, c, post("/admin/trusted/application/1", &[("accept", "Accept")]));
        let (_, _, page) = send!(app, c, get("/admin/trusted/application/1"));
        assert!(page.contains("Application has been accepted.") && page.contains("<dd>Accepted</dd>"), "{page}");
        assert!(!page.contains("name=\"accept\""), "closed applications have no decision buttons");
        assert!(User::by_id(&mut pool.get().unwrap(), 1).unwrap().unwrap().level() >= UserLevel::Trusted);
        let (_, _, page) = send!(app, c, get("/admin/trusted/closed"));
        assert!(page.contains("List of closed applications") && page.contains("Accepted"), "{page}");
    }

    #[actix_web::test]
    async fn guests_are_sent_to_login() {
        let pool = pool();
        let (app, _) = app!(pool, 1);
        let res = test::call_service(&app, get("/trusted/request").to_request()).await;
        assert_eq!(res.headers().get("Location").unwrap(), "/login");
        let res = test::call_service(&app, get("/admin/trusted").to_request()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
