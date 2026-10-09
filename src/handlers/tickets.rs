//! Support tickets: signed-in users write to the staff at /tickets and both sides reply
//! until the ticket is closed; staff work through them at /admin/tickets. Not in upstream.
//! New tickets and replies are rate limited per user (TICKET_RATE_* in .env).

use actix_session::Session;
use actix_web::http::StatusCode;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::auth::{CurrentUser, LoggedIn, Moderator, Permission};
use crate::config::{Config, TicketConfig};
use crate::db::{DbConnection, DbPool};
use crate::models::{
    user_link, validate_message, validate_subject, AdminLog, Ticket, TicketFilter, User, TICKETS_PER_PAGE,
    TICKET_CLOSED, TICKET_OPEN,
};
use crate::utils::context::base_context;
use crate::utils::pagination::Pagination;
use crate::utils::{flash, internal_error};

fn render(tmpl: &Tera, name: &str, ctx: &tera::Context, status: StatusCode) -> Result<HttpResponse> {
    let body = tmpl.render(name, ctx).map_err(internal_error)?;
    Ok(HttpResponse::build(status).content_type("text/html").body(body))
}

fn redirect(location: &str) -> HttpResponse {
    HttpResponse::Found().insert_header(("Location", location)).finish()
}

/// "24 hours", "1 hour", "90 minutes": a rate-limit window for messages.
fn describe_window(secs: i64) -> String {
    let plural = |n: i64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    match secs {
        s if s >= 3600 && s % 3600 == 0 => plural(s / 3600, "hour"),
        s if s >= 60 && s % 60 == 0 => plural(s / 60, "minute"),
        s => plural(s, "second"),
    }
}

/// Why `user` may not open another ticket right now, or `None` when they may.
fn ticket_limit(conn: &mut DbConnection, user: &User, limits: &TicketConfig) -> Result<Option<String>> {
    if limits.max_tickets <= 0 || user.can(Permission::HandleTickets) {
        return Ok(None);
    }
    let since = chrono::Utc::now().naive_utc() - chrono::Duration::seconds(limits.ticket_window_secs);
    let opened = Ticket::opened_since(conn, user.id, since).map_err(internal_error)?;
    Ok((opened >= limits.max_tickets).then(|| {
        format!(
            "You can open {} ticket{} per {}. Please wait, or add to one of your open tickets.",
            limits.max_tickets,
            if limits.max_tickets == 1 { "" } else { "s" },
            describe_window(limits.ticket_window_secs)
        )
    }))
}

/// Why `user` may not reply right now, or `None` when they may.
fn reply_limit(conn: &mut DbConnection, user: &User, limits: &TicketConfig) -> Result<Option<String>> {
    if limits.max_replies <= 0 || user.can(Permission::HandleTickets) {
        return Ok(None);
    }
    let since = chrono::Utc::now().naive_utc() - chrono::Duration::seconds(limits.reply_window_secs);
    let replies = Ticket::replies_since(conn, user.id, since).map_err(internal_error)?;
    Ok((replies >= limits.max_replies).then(|| {
        format!(
            "You can send {} repl{} per {}. Please wait a while before replying again.",
            limits.max_replies,
            if limits.max_replies == 1 { "y" } else { "ies" },
            describe_window(limits.reply_window_secs)
        )
    }))
}

/// GET /tickets: the user's own tickets.
pub async fn my_tickets(
    CurrentUser(user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let Some(user) = user else { return Ok(redirect("/login")) };
    let mut conn = pool.get().map_err(internal_error)?;
    let tickets = Ticket::of_user(&mut conn, user.id).map_err(internal_error)?;
    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("tickets", &tickets);
    render(&tmpl, "tickets.html", &ctx, StatusCode::OK)
}

#[derive(Debug, Default, Deserialize)]
pub struct NewTicketForm {
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub message: String,
}

fn new_ticket_page(
    tmpl: &Tera,
    cfg: &Config,
    user: &User,
    form: &NewTicketForm,
    errors: &serde_json::Value,
    limit: Option<String>,
    status: StatusCode,
) -> Result<HttpResponse> {
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("form", &serde_json::json!({ "subject": form.subject, "message": form.message }));
    ctx.insert("errors", errors);
    ctx.insert("limit", &limit);
    render(tmpl, "ticket_new.html", &ctx, status)
}

/// GET /tickets/new: the form, or why the user has to wait.
pub async fn new_ticket_get(
    CurrentUser(user): CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let Some(user) = user else { return Ok(redirect("/login")) };
    let mut conn = pool.get().map_err(internal_error)?;
    let limit = ticket_limit(&mut conn, &user, &cfg.tickets)?;
    new_ticket_page(&tmpl, &cfg, &user, &NewTicketForm::default(), &serde_json::json!({}), limit, StatusCode::OK)
}

/// POST /tickets/new
pub async fn new_ticket_post(
    LoggedIn(user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<NewTicketForm>,
) -> Result<HttpResponse> {
    let form = form.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;
    if let Some(limit) = ticket_limit(&mut conn, &user, &cfg.tickets)? {
        let errors = serde_json::json!({});
        return new_ticket_page(&tmpl, &cfg, &user, &form, &errors, Some(limit), StatusCode::TOO_MANY_REQUESTS);
    }
    match (validate_subject(&form.subject), validate_message(&form.message)) {
        (Ok(subject), Ok(message)) => {
            let id = Ticket::create(&mut conn, user.id, &subject, &message).map_err(internal_error)?;
            flash::push(&session, "success", "", "Your ticket has been sent. Staff will reply here.");
            Ok(redirect(&format!("/tickets/{id}")))
        }
        (subject, message) => {
            let errors = serde_json::json!({ "subject": subject.err(), "message": message.err() });
            new_ticket_page(&tmpl, &cfg, &user, &form, &errors, None, StatusCode::OK)
        }
    }
}

/// The ticket if `user` may see it: their own, or any for staff. Everyone else gets 404,
/// so ticket numbers don't reveal anything.
fn load_ticket(conn: &mut DbConnection, user: &User, id: i32) -> Result<Ticket> {
    Ticket::by_id(conn, id)
        .map_err(internal_error)?
        .filter(|t| t.user_id == user.id || user.can(Permission::HandleTickets))
        .ok_or_else(|| actix_web::error::ErrorNotFound("Ticket not found"))
}

#[allow(clippy::too_many_arguments)]
fn ticket_page(
    conn: &mut DbConnection,
    tmpl: &Tera,
    cfg: &Config,
    session: &Session,
    user: &User,
    ticket: &Ticket,
    message: &str,
    error: Option<(StatusCode, String)>,
) -> Result<HttpResponse> {
    let opener = User::by_id(conn, ticket.user_id).map_err(internal_error)?;
    let messages = ticket.messages(conn).map_err(internal_error)?;
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("flash_messages", &flash::take(session));
    ctx.insert("ticket", ticket);
    ctx.insert("opener", &opener.map(|u| u.username));
    ctx.insert("messages", &messages);
    ctx.insert("is_owner", &(ticket.user_id == user.id));
    ctx.insert("message", message);
    let (status, error) = error.map_or((StatusCode::OK, None), |(status, e)| (status, Some(e)));
    ctx.insert("error", &error);
    render(tmpl, "ticket.html", &ctx, status)
}

/// GET /tickets/{id}
pub async fn view_ticket(
    CurrentUser(user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let Some(user) = user else { return Ok(redirect("/login")) };
    let mut conn = pool.get().map_err(internal_error)?;
    let ticket = load_ticket(&mut conn, &user, path.into_inner())?;
    ticket_page(&mut conn, &tmpl, &cfg, &session, &user, &ticket, "", None)
}

#[derive(Debug, Deserialize)]
pub struct TicketActionForm {
    /// reply, close or reopen
    pub action: String,
    #[serde(default)]
    pub message: String,
}

/// POST /tickets/{id}: reply, close or reopen. The opener and staff may do all three; staff
/// actions on someone else's ticket go to the admin log.
pub async fn ticket_post(
    LoggedIn(user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
    form: web::Form<TicketActionForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let ticket = load_ticket(&mut conn, &user, path.into_inner())?;
    let here = format!("/tickets/{}", ticket.id);
    // Staff answering someone else's ticket; on their own ticket a moderator is a user
    let as_staff = ticket.user_id != user.id;
    let err = internal_error;

    let (entry, done) = match form.action.as_str() {
        "reply" => {
            let error = if !ticket.is_open() {
                Err((StatusCode::BAD_REQUEST, "This ticket is closed. Reopen it to reply.".to_string()))
            } else if let Some(limit) = reply_limit(&mut conn, &user, &cfg.tickets)? {
                Err((StatusCode::TOO_MANY_REQUESTS, limit))
            } else {
                validate_message(&form.message).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
            };
            let message = match error {
                Ok(message) => message,
                Err(e) => return ticket_page(&mut conn, &tmpl, &cfg, &session, &user, &ticket, &form.message, Some(e)),
            };
            ticket.reply(&mut conn, user.id, &message, as_staff).map_err(err)?;
            ("Replied to", "Reply sent.")
        }
        "close" if ticket.is_open() => {
            ticket.set_status(&mut conn, TICKET_CLOSED).map_err(err)?;
            ("Closed", "Ticket closed.")
        }
        "reopen" if !ticket.is_open() => {
            ticket.set_status(&mut conn, TICKET_OPEN).map_err(err)?;
            ("Reopened", "Ticket reopened.")
        }
        "close" | "reopen" => return Ok(redirect(&here)),
        _ => return Err(actix_web::error::ErrorBadRequest("Unknown action")),
    };
    if as_staff {
        let opener = User::by_id(&mut conn, ticket.user_id).map_err(err)?;
        let opener = opener.map_or_else(|| "[deleted user]".to_string(), |u| user_link(&u.username));
        let log = format!("Ticket [#{0}](/tickets/{0}): {entry} ticket by {opener}", ticket.id);
        AdminLog::add(&mut conn, user.id, &log).map_err(err)?;
    }
    flash::push(&session, "success", "", done);
    Ok(redirect(&here))
}

#[derive(Debug, Deserialize)]
pub struct QueueParams {
    pub p: Option<i64>,
}

/// GET /admin/tickets and /admin/tickets/{closed,all}: the staff queue.
pub async fn admin_tickets(
    Moderator(user): Moderator,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: Option<web::Path<String>>,
    params: web::Query<QueueParams>,
) -> Result<HttpResponse> {
    if !user.can(Permission::HandleTickets) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let list_filter = path.map(|p| p.into_inner());
    let filter =
        TicketFilter::parse(list_filter.as_deref()).ok_or_else(|| actix_web::error::ErrorNotFound("Not found"))?;
    let mut conn = pool.get().map_err(internal_error)?;
    let page = params.p.unwrap_or(1).max(1);
    let (tickets, total) = Ticket::queue(&mut conn, filter, page).map_err(internal_error)?;
    let mut ctx = base_context(&cfg, Some(&user));
    ctx.insert("flash_messages", &flash::take(&session));
    ctx.insert("tickets", &tickets);
    ctx.insert("list_filter", filter.name());
    ctx.insert("pagination", &Pagination::new(page, total, TICKETS_PER_PAGE));
    render(&tmpl, "admin/tickets.html", &ctx, StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{adminlog, support_ticket_messages, support_tickets};
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{
        cookie::{Cookie, Key},
        test, App,
    };
    use diesel::prelude::*;
    use diesel::r2d2::Pool;

    fn pool() -> DbPool {
        // One connection, so every request sees the same in-memory database
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
             (1, 'alice', 'x', 1, 0), (2, 'bob', 'x', 1, 0), (3, 'mod', 'x', 1, 2)",
        )
        .execute(&mut conn)
        .unwrap();
        pool
    }

    fn config(max_tickets: i64, max_replies: i64) -> Config {
        Config { tickets: TicketConfig { max_tickets, max_replies, ..Default::default() }, ..Config::for_tests() }
    }

    use crate::middleware::auth::test_support::login;

    macro_rules! app {
        ($pool:expr, $cfg:expr, $user:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            let app = test::init_service(
                App::new()
                    .app_data(web::Data::new($cfg))
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(tera))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/login/{id}", web::get().to(login))
                    .route("/tickets", web::get().to(my_tickets))
                    .route("/tickets/new", web::get().to(new_ticket_get))
                    .route("/tickets/new", web::post().to(new_ticket_post))
                    .route("/tickets/{id}", web::get().to(view_ticket))
                    .route("/tickets/{id}", web::post().to(ticket_post))
                    .route("/admin/tickets", web::get().to(admin_tickets))
                    .route("/admin/tickets/{list_filter}", web::get().to(admin_tickets)),
            )
            .await;
            let cookie: Option<Cookie<'static>> = match $user {
                Some(id) => {
                    let req = test::TestRequest::get().uri(&format!("/login/{}", id)).to_request();
                    let res = test::call_service(&app, req).await;
                    res.response().cookies().next().map(|c| c.into_owned())
                }
                None => None,
            };
            (app, cookie)
        }};
    }

    fn with_cookie(mut req: test::TestRequest, cookie: &Option<Cookie<'static>>) -> test::TestRequest {
        if let Some(c) = cookie {
            req = req.cookie(c.clone());
        }
        req
    }

    /// Sends a request; returns the status and body, keeping the session cookie it set.
    macro_rules! send {
        ($app:expr, $req:expr, $cookie:expr) => {{
            let res = test::call_service(&$app, with_cookie($req, $cookie).to_request()).await;
            if let Some(c) = res.response().cookies().next() {
                *$cookie = Some(c.into_owned());
            }
            let status = res.status();
            (status, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
        }};
    }

    fn get(uri: &str) -> test::TestRequest {
        test::TestRequest::get().uri(uri)
    }

    fn post(uri: &str, form: &[(&str, &str)]) -> test::TestRequest {
        test::TestRequest::post().uri(uri).set_form(form)
    }

    fn count(pool: &DbPool) -> (i64, i64) {
        let conn = &mut pool.get().unwrap();
        (
            support_tickets::table.count().get_result(conn).unwrap(),
            support_ticket_messages::table.count().get_result(conn).unwrap(),
        )
    }

    fn log_lines(pool: &DbPool) -> Vec<String> {
        adminlog::table.select(adminlog::log).load(&mut pool.get().unwrap()).unwrap()
    }

    #[actix_web::test]
    async fn guests_are_sent_to_login() {
        let pool = pool();
        let (app, mut cookie) = app!(pool, config(3, 20), None::<i32>);
        for uri in ["/tickets", "/tickets/new", "/tickets/1"] {
            let res = test::call_service(&app, get(uri).to_request()).await;
            assert_eq!(res.status(), StatusCode::FOUND, "{uri}");
            assert_eq!(res.headers().get("Location").unwrap(), "/login");
        }
        let (status, _) =
            send!(app, post("/tickets/new", &[("subject", "Hello"), ("message", "Hi there")]), &mut cookie);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(count(&pool), (0, 0));
    }

    #[actix_web::test]
    async fn user_opens_a_ticket_and_staff_answers() {
        let pool = pool();
        let (app, mut cookie) = app!(pool, config(3, 20), Some(1));
        // Validation keeps what was typed
        let (status, html) =
            send!(app, post("/tickets/new", &[("subject", "ab"), ("message", "Some <text>")]), &mut cookie);
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Subject must be at least 3"), "{html}");
        assert!(html.contains("Some &lt;text&gt;"), "{html}");
        assert_eq!(count(&pool), (0, 0));

        let form = [("subject", "Account question"), ("message", "Line one\r\nLine two")];
        let (status, _) = send!(app, post("/tickets/new", &form), &mut cookie);
        assert_eq!(status, StatusCode::FOUND);
        let (_, html) = send!(app, get("/tickets/1"), &mut cookie);
        assert!(html.contains("Your ticket has been sent"), "{html}");
        assert!(html.contains("Account question") && html.contains("Line one\nLine two"), "{html}");
        assert!(html.contains("Waiting for staff"), "{html}");
        let (_, html) = send!(app, get("/tickets"), &mut cookie);
        assert!(html.contains("<a href=\"/tickets/1\">Account question</a>"), "{html}");

        // Other users don't see it; staff do
        let (app2, mut bob) = app!(pool, config(3, 20), Some(2));
        let (status, _) = send!(app2, get("/tickets/1"), &mut bob);
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send!(app2, post("/tickets/1", &[("action", "close")]), &mut bob);
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, html) = send!(app2, get("/tickets"), &mut bob);
        assert!(html.contains("You have no tickets."), "{html}");

        let (app3, mut moder) = app!(pool, config(3, 20), Some(3));
        let (_, html) = send!(app3, get("/admin/tickets"), &mut moder);
        assert!(html.contains("<a href=\"/tickets/1\">Account question</a>"), "{html}");
        assert!(html.contains("<a href=\"/user/alice\">alice</a>"), "{html}");
        let (status, _) = send!(app3, post("/tickets/1", &[("action", "reply"), ("message", "Fixed it")]), &mut moder);
        assert_eq!(status, StatusCode::FOUND);
        let (status, _) = send!(app3, post("/tickets/1", &[("action", "close")]), &mut moder);
        assert_eq!(status, StatusCode::FOUND);
        assert_eq!(
            log_lines(&pool),
            vec![
                "Ticket [#1](/tickets/1): Replied to ticket by [alice](/user/alice)",
                "Ticket [#1](/tickets/1): Closed ticket by [alice](/user/alice)",
            ]
        );
        let (_, html) = send!(app3, get("/admin/tickets"), &mut moder);
        assert!(html.contains("No tickets."), "{html}");
        let (_, html) = send!(app3, get("/admin/tickets/closed"), &mut moder);
        assert!(html.contains("Account question"), "{html}");

        // The user sees the staff reply, can't reply while closed, and can reopen
        let (_, html) = send!(app, get("/tickets/1"), &mut cookie);
        assert!(html.contains("Fixed it") && html.contains("label-info\">Staff"), "{html}");
        assert!(html.contains("This ticket is closed."), "{html}");
        let (status, _) =
            send!(app, post("/tickets/1", &[("action", "reply"), ("message", "Still broken")]), &mut cookie);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        send!(app, post("/tickets/1", &[("action", "reopen")]), &mut cookie);
        let (status, _) =
            send!(app, post("/tickets/1", &[("action", "reply"), ("message", "Still broken")]), &mut cookie);
        assert_eq!(status, StatusCode::FOUND);
        let ticket = Ticket::by_id(&mut pool.get().unwrap(), 1).unwrap().unwrap();
        assert!(ticket.is_open() && !ticket.staff_replied);
        // The user's own actions aren't moderator actions
        assert_eq!(log_lines(&pool).len(), 2);
        assert_eq!(count(&pool), (1, 3));
    }

    #[actix_web::test]
    async fn queue_is_for_staff_only() {
        let pool = pool();
        for (user, expected) in [(None, StatusCode::UNAUTHORIZED), (Some(1), StatusCode::FORBIDDEN)] {
            let (app, mut cookie) = app!(pool, config(3, 20), user);
            let (status, _) = send!(app, get("/admin/tickets"), &mut cookie);
            assert_eq!(status, expected, "{user:?}");
        }
        let (app, mut cookie) = app!(pool, config(3, 20), Some(3));
        let (status, _) = send!(app, get("/admin/tickets/bogus"), &mut cookie);
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn new_tickets_and_replies_are_rate_limited() {
        let pool = pool();
        let (app, mut cookie) = app!(pool, config(2, 1), Some(1));
        for n in 1..=2 {
            let (status, _) =
                send!(app, post("/tickets/new", &[("subject", "Question"), ("message", "Some message")]), &mut cookie);
            assert_eq!(status, StatusCode::FOUND, "ticket {n}");
        }
        let (status, html) =
            send!(app, post("/tickets/new", &[("subject", "Question"), ("message", "Some message")]), &mut cookie);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(html.contains("You can open 2 tickets per 24 hours."), "{html}");
        let (_, html) = send!(app, get("/tickets/new"), &mut cookie);
        assert!(html.contains("You can open 2 tickets per 24 hours.") && !html.contains("<textarea"), "{html}");

        // The opening messages don't count as replies
        let (status, _) = send!(app, post("/tickets/1", &[("action", "reply"), ("message", "More info")]), &mut cookie);
        assert_eq!(status, StatusCode::FOUND);
        let (status, html) =
            send!(app, post("/tickets/2", &[("action", "reply"), ("message", "Even more")]), &mut cookie);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(html.contains("You can send 1 reply per 1 hour.") && html.contains("Even more"), "{html}");
        assert_eq!(count(&pool), (2, 3));

        // Staff aren't limited
        let (app, mut moder) = app!(pool, config(1, 1), Some(3));
        for _ in 0..2 {
            let (status, _) = send!(app, post("/tickets/1", &[("action", "reply"), ("message", "Answer")]), &mut moder);
            assert_eq!(status, StatusCode::FOUND);
        }
    }

    #[::core::prelude::v1::test]
    fn windows_read_naturally() {
        assert_eq!(describe_window(86400), "24 hours");
        assert_eq!(describe_window(3600), "1 hour");
        assert_eq!(describe_window(90 * 60), "90 minutes");
        assert_eq!(describe_window(45), "45 seconds");
    }
}
