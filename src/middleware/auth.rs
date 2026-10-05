use actix_session::Session;
use actix_web::web;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::SqliteConnection;
use crate::models::User;

pub const SESSION_USER_KEY: &str = "user_id";

pub fn get_current_user(session: &Session, pool: &Pool<ConnectionManager<SqliteConnection>>) -> Option<User> {
    let user_id: i32 = session.get(SESSION_USER_KEY).ok()??;
    let mut conn = pool.get().ok()?;
    // Banned or deactivated users lose access immediately, not at next login
    User::by_id(&mut conn, user_id).ok()?.filter(|u| u.is_active())
}

pub fn login_user(session: &Session, user_id: i32) -> Result<(), actix_session::SessionInsertError> {
    session.insert(SESSION_USER_KEY, user_id)
}

pub fn logout_user(session: &Session) {
    session.remove(SESSION_USER_KEY);
    session.purge();
}
