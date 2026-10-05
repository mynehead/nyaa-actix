use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;

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
