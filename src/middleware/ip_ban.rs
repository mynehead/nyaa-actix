//! Upstream's `before_request` ban check: a POST from a banned IP signs the user out,
//! or gets "You are banned." when nobody is signed in.

use actix_session::SessionExt;
use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{http::Method, web, Error, HttpResponse};

use crate::db::DbPool;
use crate::middleware::auth::{logout_user, SESSION_USER_KEY};
use crate::models::Ban;
use crate::utils::pack_ip;

pub async fn reject_banned_ip(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    if req.method() == Method::POST {
        let ip = req.peer_addr().map(|a| pack_ip(a.ip()));
        let pool = req.app_data::<web::Data<DbPool>>().cloned();
        if let (Some(ip), Some(pool)) = (ip, pool) {
            let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
            let banned = Ban::ip_banned(&mut conn, &ip).map_err(actix_web::error::ErrorInternalServerError)?;
            if banned {
                let session = req.get_session();
                let response = if session.get::<i32>(SESSION_USER_KEY).ok().flatten().is_some() {
                    // Upstream's logout(): sign out and go home
                    logout_user(&session);
                    HttpResponse::Found().insert_header(("Location", "/")).finish()
                } else {
                    HttpResponse::Forbidden().body("You are banned.")
                };
                return Ok(req.into_response(response));
            }
        }
    }
    next.call(req).await.map(ServiceResponse::map_into_boxed_body)
}
