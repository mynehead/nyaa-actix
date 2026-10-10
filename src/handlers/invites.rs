//! /invites: invite codes for REGISTRATION_MODE=invite. Moderators make as many as they
//! like and see everyone's; other users make the ones they were given (see
//! [`Invite::remaining`]) and see their own.

use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

use crate::auth::policy::can_ban;
use crate::auth::{LoggedIn, Permission};
use crate::config::Config;
use crate::db::DbPool;
use crate::handlers::account::looks_like_email;
use crate::handlers::admin::{PageParams, ADMIN_PER_PAGE};
use crate::models::{new_invite_code, user_link, AdminLog, Invite, NewInvite, User};
use crate::utils::context::base_context;
use crate::utils::pagination::Pagination;
use crate::utils::{flash, internal_error};

fn back_to_invites() -> HttpResponse {
    HttpResponse::SeeOther().insert_header(("Location", "/invites")).finish()
}

/// The invites page; `new_link` is the register link of a code just made, shown only now.
fn render(
    tmpl: &Tera,
    cfg: &Config,
    pool: &DbPool,
    user: &User,
    session: &Session,
    page: i64,
    new_link: Option<&str>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let now = chrono::Utc::now().naive_utc();
    let everyone = user.can(Permission::CreateInvites);
    let (invites, total) =
        Invite::page(&mut conn, (!everyone).then_some(user.id), page, ADMIN_PER_PAGE, now).map_err(internal_error)?;
    let remaining =
        Invite::remaining(&mut conn, user, cfg.registration.invites_for_trusted, now).map_err(internal_error)?;
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("invites", &invites);
    ctx.insert("invites_remaining", &remaining);
    ctx.insert("invite_expiry_days", &cfg.registration.invite_expiry_days);
    ctx.insert("new_invite_link", &new_link);
    ctx.insert("pagination", &Pagination::new(page, total, ADMIN_PER_PAGE));
    ctx.insert("flash_messages", &flash::take(session));
    let html = tmpl.render("invites.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn list(
    LoggedIn(current_user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    query: web::Query<PageParams>,
) -> Result<HttpResponse> {
    render(&tmpl, &cfg, &pool, &current_user, &session, query.page(), None)
}

#[derive(Debug, Deserialize)]
pub struct InviteForm {
    /// Optional: only this address may use the code.
    #[serde(default)]
    email: String,
}

/// Makes a code and shows its register link once; only its hash is kept.
pub async fn create(
    LoggedIn(current_user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    form: web::Form<InviteForm>,
) -> Result<HttpResponse> {
    let email = form.email.trim();
    if !email.is_empty() && !looks_like_email(email) {
        flash::push(&session, "danger", "", "Please enter a valid email address, or leave it empty.");
        return Ok(back_to_invites());
    }
    let (code, code_hash) = new_invite_code();
    let now = chrono::Utc::now().naive_utc();
    let invite = NewInvite {
        code_hash,
        inviter_id: current_user.id,
        email: (!email.is_empty()).then(|| email.to_owned()),
        created_time: now,
        expires_time: now + chrono::Duration::days(cfg.registration.invite_expiry_days),
    };
    let mut conn = pool.get().map_err(internal_error)?;
    let made = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            let remaining = Invite::remaining(conn, &current_user, cfg.registration.invites_for_trusted, now)?;
            if remaining == Some(0) {
                return Ok(false);
            }
            Invite::insert(conn, &invite)?;
            if current_user.can(Permission::CreateInvites) {
                let for_whom = if email.is_empty() { String::new() } else { " for a fixed email address".into() };
                AdminLog::add(conn, current_user.id, &format!("Created an invite code{for_whom}"))?;
            }
            Ok(true)
        })
        .map_err(internal_error)?;
    drop(conn);
    if !made {
        flash::push(&session, "danger", "", "You have no invites left.");
        return Ok(back_to_invites());
    }
    let link = format!("{}/register?invite={code}", cfg.site_url);
    render(&tmpl, &cfg, &pool, &current_user, &session, 1, Some(&link))
}

/// Revokes an unused code: moderators any, everyone else their own. The invite comes back.
pub async fn revoke(
    LoggedIn(current_user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let invite = Invite::by_id(&mut conn, path.into_inner())
        .map_err(internal_error)?
        .filter(|i| i.inviter_id == current_user.id || current_user.can(Permission::CreateInvites))
        .ok_or_else(|| actix_web::error::ErrorNotFound("Invite not found"))?;
    let revoked = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            if !Invite::revoke(conn, invite.id)? {
                return Ok(false);
            }
            if current_user.can(Permission::CreateInvites) {
                AdminLog::add(conn, current_user.id, &format!("Revoked invite #{}", invite.id))?;
            }
            Ok(true)
        })
        .map_err(internal_error)?;
    if revoked {
        flash::push(&session, "success", "", &format!("Revoked invite #{}", invite.id));
    } else {
        flash::push(&session, "danger", "", "That invite has already been used.");
    }
    Ok(back_to_invites())
}

#[derive(Debug, Deserialize)]
pub struct GiveForm {
    amount: i32,
}

/// The "Give invites" form on a user's page: adds to (or, negative, takes from) what
/// moderators gave them.
pub async fn give(
    LoggedIn(moderator): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
    form: web::Form<GiveForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let user = User::by_username(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;
    if !moderator.can(Permission::CreateInvites) || !can_ban(&moderator, &user) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let back = HttpResponse::SeeOther()
        .insert_header(("Location", format!("/user/{}", urlencoding::encode(&user.username))))
        .finish();
    if form.amount == 0 || form.amount.abs() > 100 {
        flash::push(
            &session,
            "danger",
            "",
            "Give between 1 and 100 invites, or take some back with a negative number.",
        );
        return Ok(back);
    }
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        Invite::give(conn, user.id, form.amount)?;
        let what = if form.amount > 0 {
            format!("Gave {} {} invite(s)", user_link(&user.username), form.amount)
        } else {
            format!("Took {} invite(s) back from {}", -form.amount, user_link(&user.username))
        };
        AdminLog::add(conn, moderator.id, &what)
    })
    .map_err(internal_error)?;
    flash::push(&session, "success", "", "Invites updated.");
    Ok(back)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::invites;
    use crate::middleware::auth::test_support::login;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{
        cookie::{Cookie, Key},
        http::StatusCode,
        test, App,
    };
    use diesel::r2d2::Pool;

    fn pool() -> DbPool {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
                           (1, 'regular', 'x', 1, 0), (2, 'mod', 'x', 1, 2), (3, 'trust', 'x', 1, 1)",
        )
        .execute(&mut conn)
        .unwrap();
        pool
    }

    macro_rules! app {
        ($pool:expr, $user:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            let mut cfg = Config::for_tests();
            cfg.site_url = "https://nyaa.test".into();
            let app = test::init_service(
                App::new()
                    .app_data(web::Data::new(cfg))
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(tera))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/login/{id}", web::get().to(login))
                    .route("/invites", web::get().to(list))
                    .route("/invites", web::post().to(create))
                    .route("/invites/{id}/revoke", web::post().to(revoke))
                    .route("/user/{username}/invites", web::post().to(give)),
            )
            .await;
            let res =
                test::call_service(&app, test::TestRequest::get().uri(&format!("/login/{}", $user)).to_request()).await;
            let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
            (app, cookie)
        }};
    }

    fn all(pool: &DbPool) -> Vec<Invite> {
        invites::table.select(Invite::as_select()).load(&mut pool.get().unwrap()).unwrap()
    }

    #[actix_web::test]
    async fn moderators_make_and_revoke_codes() {
        let pool = pool();
        let (app, cookie) = app!(pool, 2);
        let req =
            test::TestRequest::post().uri("/invites").cookie(cookie.clone()).set_form([("email", "")]).to_request();
        let res = test::call_service(&app, req).await;
        let status = res.status();
        let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
        assert_eq!(status, StatusCode::OK, "{page}");
        // Tera escapes the slashes in the value attribute
        let page = page.replace("&#x2F;", "/");
        let start =
            page.find("https://nyaa.test/register?invite=").expect(&page) + "https://nyaa.test/register?invite=".len();
        let code: String =
            page[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).collect();
        assert_eq!(code.len(), 22);
        let stored = all(&pool);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].inviter_id, 2);
        let hash: String = invites::table.select(invites::code_hash).first(&mut pool.get().unwrap()).unwrap();
        assert_eq!(hash, crate::models::hash_invite_code(&code), "only the hash is kept");
        let now = chrono::Utc::now().naive_utc();
        assert!(Invite::usable_by_code(&mut pool.get().unwrap(), &code, now).unwrap().is_some());

        let req = test::TestRequest::post()
            .uri(&format!("/invites/{}/revoke", stored[0].id))
            .cookie(cookie.clone())
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
        assert!(Invite::usable_by_code(&mut pool.get().unwrap(), &code, now).unwrap().is_none());
        let req = test::TestRequest::get().uri("/invites").cookie(cookie).to_request();
        let page = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(page.contains("Revoked invite #1") && page.contains("<td>revoked</td>"), "{page}");
    }

    /// POSTs `uri` with `form` as the cookie's user; gives the status.
    macro_rules! post {
        ($app:expr, $cookie:expr, $uri:expr, $form:expr) => {
            test::call_service(
                &$app,
                test::TestRequest::post().uri($uri).cookie($cookie.clone()).set_form($form).to_request(),
            )
            .await
            .status()
        };
    }

    fn made_by(pool: &DbPool, id: i32) -> usize {
        all(pool).iter().filter(|i| i.inviter_id == id).count()
    }

    #[actix_web::test]
    async fn trusted_users_get_two_and_moderators_give_more() {
        let pool = pool();
        let no_email = [("email", "")];

        // Regular users have none until a moderator gives them some
        let (app, regular) = app!(pool, 1);
        let req = test::TestRequest::get().uri("/invites").cookie(regular.clone()).to_request();
        let page = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(page.contains("<strong>0</strong> invites left") && !page.contains("Create invite"), "{page}");
        assert_eq!(post!(app, regular, "/invites", no_email), StatusCode::SEE_OTHER);
        assert_eq!(made_by(&pool, 1), 0);
        assert_eq!(post!(app, regular, "/user/regular/invites", [("amount", "5")]), StatusCode::FORBIDDEN);

        // Trusted: two, then no more; a revoked one comes back
        let (app, trusted) = app!(pool, 3);
        assert_eq!(post!(app, trusted, "/invites", no_email), StatusCode::OK);
        assert_eq!(post!(app, trusted, "/invites", no_email), StatusCode::OK);
        assert_eq!(post!(app, trusted, "/invites", no_email), StatusCode::SEE_OTHER);
        assert_eq!(made_by(&pool, 3), 2);
        let own = all(&pool)[0].id;
        assert_eq!(post!(app, trusted, &format!("/invites/{own}/revoke"), no_email), StatusCode::SEE_OTHER);
        assert_eq!(post!(app, trusted, "/invites", no_email), StatusCode::OK);
        assert_eq!(made_by(&pool, 3), 3);

        // Moderators give invites on the user page; only their own codes are others' business
        let (app, moderator) = app!(pool, 2);
        assert_eq!(post!(app, moderator, "/user/regular/invites", [("amount", "1")]), StatusCode::SEE_OTHER);
        assert_eq!(post!(app, moderator, "/user/regular/invites", [("amount", "0")]), StatusCode::SEE_OTHER);
        assert_eq!(post!(app, moderator, "/invites", no_email), StatusCode::OK);
        let mods = all(&pool).into_iter().find(|i| i.inviter_id == 2).unwrap().id;
        let (app, regular) = app!(pool, 1);
        assert_eq!(post!(app, regular, &format!("/invites/{mods}/revoke"), no_email), StatusCode::NOT_FOUND);
        assert_eq!(post!(app, regular, "/invites", no_email), StatusCode::OK);
        assert_eq!(post!(app, regular, "/invites", no_email), StatusCode::SEE_OTHER);
        assert_eq!(made_by(&pool, 1), 1);
        let req = test::TestRequest::get().uri("/invites").cookie(regular.clone()).to_request();
        let page = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(!page.contains("Invited by"), "only their own codes: {page}");
    }
}
