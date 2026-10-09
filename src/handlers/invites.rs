//! /admin/invites: invite codes for REGISTRATION_MODE=invite.

use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use diesel::prelude::*;
use serde::Deserialize;
use tera::Tera;

use crate::auth::{LoggedIn, Permission};
use crate::config::Config;
use crate::db::DbPool;
use crate::handlers::account::looks_like_email;
use crate::handlers::admin::{PageParams, ADMIN_PER_PAGE};
use crate::models::{new_invite_code, AdminLog, Invite, NewInvite, User};
use crate::utils::context::base_context;
use crate::utils::pagination::Pagination;
use crate::utils::{flash, internal_error};

fn may_invite(user: &User) -> Result<()> {
    if user.can(Permission::CreateInvites) {
        Ok(())
    } else {
        Err(actix_web::error::ErrorForbidden("Not allowed"))
    }
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
    let (invites, total) = Invite::page(&mut conn, page, ADMIN_PER_PAGE, now).map_err(internal_error)?;
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("invites", &invites);
    ctx.insert("invite_expiry_days", &cfg.registration.invite_expiry_days);
    ctx.insert("new_invite_link", &new_link);
    ctx.insert("pagination", &Pagination::new(page, total, ADMIN_PER_PAGE));
    ctx.insert("flash_messages", &flash::take(session));
    let html = tmpl.render("admin/invites.html", &ctx).map_err(internal_error)?;
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
    may_invite(&current_user)?;
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
    may_invite(&current_user)?;
    let email = form.email.trim();
    if !email.is_empty() && !looks_like_email(email) {
        flash::push(&session, "danger", "", "Please enter a valid email address, or leave it empty.");
        return Ok(HttpResponse::SeeOther().insert_header(("Location", "/admin/invites")).finish());
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
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        Invite::insert(conn, &invite)?;
        let for_whom = if email.is_empty() { String::new() } else { " for a fixed email address".into() };
        AdminLog::add(conn, current_user.id, &format!("Created an invite code{for_whom}"))
    })
    .map_err(internal_error)?;
    drop(conn);
    let link = format!("{}/register?invite={code}", cfg.site_url);
    render(&tmpl, &cfg, &pool, &current_user, &session, 1, Some(&link))
}

/// Revokes an unused code.
pub async fn revoke(
    LoggedIn(current_user): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    may_invite(&current_user)?;
    let mut conn = pool.get().map_err(internal_error)?;
    let invite = Invite::by_id(&mut conn, path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Invite not found"))?;
    let revoked = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            if !Invite::revoke(conn, invite.id)? {
                return Ok(false);
            }
            AdminLog::add(conn, current_user.id, &format!("Revoked invite #{}", invite.id))?;
            Ok(true)
        })
        .map_err(internal_error)?;
    if revoked {
        flash::push(&session, "success", "", &format!("Revoked invite #{}", invite.id));
    } else {
        flash::push(&session, "danger", "", "That invite has already been used.");
    }
    Ok(HttpResponse::SeeOther().insert_header(("Location", "/admin/invites")).finish())
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
                           (1, 'regular', 'x', 1, 0), (2, 'mod', 'x', 1, 2)",
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
                    .route("/admin/invites", web::get().to(list))
                    .route("/admin/invites", web::post().to(create))
                    .route("/admin/invites/{id}/revoke", web::post().to(revoke)),
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
        let (app, cookie) = app!(pool, 1);
        let req = test::TestRequest::get().uri("/admin/invites").cookie(cookie.clone()).to_request();
        assert_eq!(test::call_service(&app, req).await.status(), StatusCode::FORBIDDEN);
        let req = test::TestRequest::post().uri("/admin/invites").cookie(cookie).set_form([("email", "")]).to_request();
        assert_eq!(test::call_service(&app, req).await.status(), StatusCode::FORBIDDEN);
        assert!(all(&pool).is_empty());

        let (app, cookie) = app!(pool, 2);
        let req = test::TestRequest::post()
            .uri("/admin/invites")
            .cookie(cookie.clone())
            .set_form([("email", "")])
            .to_request();
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
            .uri(&format!("/admin/invites/{}/revoke", stored[0].id))
            .cookie(cookie.clone())
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
        assert!(Invite::usable_by_code(&mut pool.get().unwrap(), &code, now).unwrap().is_none());
        let req = test::TestRequest::get().uri("/admin/invites").cookie(cookie).to_request();
        let page = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(page.contains("Revoked invite #1") && page.contains("<td>revoked</td>"), "{page}");
    }
}
