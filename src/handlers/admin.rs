use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use tera::Tera;
use crate::config::Config;
use crate::db::DbPool;
use crate::utils::context::base_context;
use crate::middleware::auth::get_current_user;

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

pub async fn log(
    session: Session,
    pool: web::Data<DbPool>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    if !current_user.is_moderator() {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok(HttpResponse::Ok().body("<h1>Admin Log</h1><p>Not yet implemented.</p>"))
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
