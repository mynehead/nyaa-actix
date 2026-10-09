//! MFA_REQUIRED_LEVEL: signed-in users of that level and up who have no two-factor set up
//! are sent to /profile/2fa until they do. Only that page, logging out and static files
//! stay reachable. Does nothing (no database work) while the setting is off.

use actix_session::SessionExt;
use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpResponse};

use crate::auth::mfa::UserMfa;
use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::utils::internal_error;

pub const SETUP_URL: &str = "/profile/2fa";

/// Paths a user who still has to set up two-factor may use.
fn allowed(path: &str) -> bool {
    path == "/logout" || path.starts_with(SETUP_URL) || path.starts_with("/static/") || path.starts_with("/avatar/")
}

pub async fn require_two_factor(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    let cfg = req.app_data::<web::Data<Config>>().cloned();
    let pool = req.app_data::<web::Data<DbPool>>().cloned();
    if let (Some(cfg), Some(pool)) = (cfg, pool) {
        if cfg.mfa.required_level.is_some() && !allowed(req.path()) {
            let user = get_current_user(&req.get_session(), &pool);
            if let Some(user) = user.filter(|u| cfg.mfa.required_for(u.level())) {
                let mut conn = pool.get().map_err(internal_error)?;
                if !UserMfa::is_enabled(&mut conn, user.id).map_err(internal_error)? {
                    let response = HttpResponse::SeeOther().insert_header(("Location", SETUP_URL)).finish();
                    return Ok(req.into_response(response));
                }
            }
        }
    }
    next.call(req).await.map(ServiceResponse::map_into_boxed_body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_page_and_logout_stay_reachable() {
        for path in ["/profile/2fa", "/profile/2fa/enable", "/logout", "/static/css/main.css"] {
            assert!(allowed(path), "{path}");
        }
        for path in ["/", "/profile", "/upload", "/admin/log", "/user/x"] {
            assert!(!allowed(path), "{path}");
        }
    }
}
