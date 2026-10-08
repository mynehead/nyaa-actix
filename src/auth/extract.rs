//! Handler arguments that load the signed-in user, so handlers don't each repeat the
//! session lookup and the 401/403 answers.

use std::future::{ready, Ready};

use actix_session::SessionExt;
use actix_web::dev::Payload;
use actix_web::{error, web, Error, FromRequest, HttpRequest};

use super::Permission;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::User;

fn current_user(req: &HttpRequest) -> Option<User> {
    let pool = req.app_data::<web::Data<DbPool>>()?;
    get_current_user(&req.get_session(), pool)
}

/// The signed-in user, or `None` for guests.
pub struct CurrentUser(pub Option<User>);

impl FromRequest for CurrentUser {
    type Error = Error;
    type Future = Ready<Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(Ok(CurrentUser(current_user(req))))
    }
}

/// The signed-in user; guests get 401.
pub struct LoggedIn(pub User);

impl FromRequest for LoggedIn {
    type Error = Error;
    type Future = Ready<Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(current_user(req).map(LoggedIn).ok_or_else(|| error::ErrorUnauthorized("Login required")))
    }
}

/// A user allowed on the admin pages ([`Permission::ViewAdminPages`]); guests get 401,
/// everyone else 403.
pub struct Moderator(pub User);

impl FromRequest for Moderator {
    type Error = Error;
    type Future = Ready<Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(match current_user(req) {
            None => Err(error::ErrorUnauthorized("Login required")),
            Some(u) if u.can(Permission::ViewAdminPages) => Ok(Moderator(u)),
            Some(_) => Err(error::ErrorForbidden("Not allowed")),
        })
    }
}
