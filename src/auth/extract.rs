//! Handler arguments that load the signed-in user, so handlers don't each repeat the
//! session lookup and the 401/403 answers.

use std::future::Future;

use actix_session::SessionExt;
use actix_web::dev::Payload;
use actix_web::{error, web, Error, FromRequest, HttpRequest};
use futures_util::future::LocalBoxFuture;

use super::Permission;
use crate::db::DbPool;
use crate::middleware::auth::load_current_user;
use crate::models::User;

/// Looks the user up off the request threads (the session lookup is a database query).
fn current_user(req: &HttpRequest) -> impl Future<Output = Option<User>> + 'static {
    let (session, pool) = (req.get_session(), req.app_data::<web::Data<DbPool>>().cloned());
    async move { load_current_user(&session, &*pool?).await }
}

/// The signed-in user, or `None` for guests.
pub struct CurrentUser(pub Option<User>);

impl FromRequest for CurrentUser {
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let user = current_user(req);
        Box::pin(async move { Ok(CurrentUser(user.await)) })
    }
}

/// The signed-in user; guests get 401.
pub struct LoggedIn(pub User);

impl FromRequest for LoggedIn {
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let user = current_user(req);
        Box::pin(async move { user.await.map(LoggedIn).ok_or_else(|| error::ErrorUnauthorized("Login required")) })
    }
}

/// A user allowed on the admin pages ([`Permission::ViewAdminPages`]); guests get 401,
/// everyone else 403.
pub struct Moderator(pub User);

impl FromRequest for Moderator {
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Error>>;

    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        let user = current_user(req);
        Box::pin(async move {
            match user.await {
                None => Err(error::ErrorUnauthorized("Login required")),
                Some(u) if u.can(Permission::ViewAdminPages) => Ok(Moderator(u)),
                Some(_) => Err(error::ErrorForbidden("Not allowed")),
            }
        })
    }
}
