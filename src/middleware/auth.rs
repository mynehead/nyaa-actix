//! Logins are server-side sessions. The (signed, encrypted) cookie only carries a random
//! session id; the `user_sessions` row is the real session, so logging out, or deleting the
//! row, ends it even if the cookie was copied.

use actix_session::{Session, SessionExt};
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{web, Error};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use chrono::{Duration, NaiveDateTime, Utc};
use diesel::prelude::*;

use crate::db::schema::{user_sessions, users};
use crate::db::{DbConnection, DbPool};
use crate::models::User;

pub const SESSION_ID_KEY: &str = "sid";

/// How a session was signed in, stored in `user_sessions.auth_method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    /// Password, then a code from an authenticator app.
    PasswordTotp,
    /// Password, then a one-time recovery code.
    PasswordRecovery,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Password => "password",
            AuthMethod::PasswordTotp => "password+totp",
            AuthMethod::PasswordRecovery => "password+recovery",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "password+totp" => AuthMethod::PasswordTotp,
            "password+recovery" => AuthMethod::PasswordRecovery,
            _ => AuthMethod::Password,
        }
    }
}

/// Sessions end after this long without a request (upstream: 7 days, renewed on activity).
pub const SESSION_TTL_DAYS: i64 = 7;

/// How stale `last_seen` may get before a request refreshes it, so most requests don't write.
const LAST_SEEN_REFRESH_MINUTES: i64 = 10;

fn now() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// Sessions last seen before this are expired.
fn expiry_cutoff() -> NaiveDateTime {
    now() - Duration::days(SESSION_TTL_DAYS)
}

pub fn get_current_user(session: &Session, pool: &DbPool) -> Option<User> {
    let sid: String = session.get(SESSION_ID_KEY).ok()??;
    let mut conn = pool.get().ok()?;
    user_by_session(&mut conn, &sid).ok()?
}

/// The active user behind an unexpired session. Banned or deactivated users lose access
/// immediately, not at next login.
fn user_by_session(conn: &mut DbConnection, sid: &str) -> QueryResult<Option<User>> {
    let user = user_sessions::table
        .inner_join(users::table)
        .filter(user_sessions::id.eq(sid))
        .filter(user_sessions::last_seen.gt(expiry_cutoff()))
        .select(User::as_select())
        .first::<User>(conn)
        .optional()?;
    Ok(user.filter(|u| u.is_active()))
}

/// Starts a fresh session (new cookie, new row) and records the login like upstream:
/// `last_login_date` and `last_login_ip`. Login handlers go through
/// `handlers::account::complete_login`, which asks for the second factor first when needed.
pub fn login_user(
    session: &Session,
    conn: &mut DbConnection,
    user_id: i32,
    ip: Option<Vec<u8>>,
    method: AuthMethod,
) -> anyhow::Result<()> {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let sid = hex::encode(bytes);
    let now = now();

    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        // Housekeeping: expired rows are never valid again
        diesel::delete(user_sessions::table.filter(user_sessions::last_seen.le(expiry_cutoff()))).execute(conn)?;
        diesel::insert_into(user_sessions::table)
            .values((
                user_sessions::id.eq(&sid),
                user_sessions::user_id.eq(user_id),
                user_sessions::created_time.eq(now),
                user_sessions::last_seen.eq(now),
                user_sessions::ip.eq(&ip),
                user_sessions::auth_method.eq(method.as_str()),
            ))
            .execute(conn)?;
        diesel::update(users::table.find(user_id))
            .set((users::last_login_date.eq(Some(now)), users::last_login_ip.eq(&ip)))
            .execute(conn)?;
        Ok(())
    })?;

    // A new cookie on login, so a session planted before login is useless
    session.renew();
    session.insert(SESSION_ID_KEY, sid)?;
    Ok(())
}

/// How the current session was signed in; Password when there is none.
pub fn session_auth_method(session: &Session, conn: &mut DbConnection) -> AuthMethod {
    let Ok(Some(sid)) = session.get::<String>(SESSION_ID_KEY) else {
        return AuthMethod::Password;
    };
    let method: Option<String> =
        user_sessions::table.find(sid).select(user_sessions::auth_method).first(conn).optional().ok().flatten();
    method.map_or(AuthMethod::Password, |m| AuthMethod::parse(&m))
}

/// Ends the session for good: deletes its row and clears the cookie.
pub fn logout_user(session: &Session, pool: &DbPool) {
    if let (Ok(Some(sid)), Ok(mut conn)) = (session.get::<String>(SESSION_ID_KEY), pool.get()) {
        if let Err(e) = diesel::delete(user_sessions::table.find(sid)).execute(&mut conn) {
            log::error!("Failed to delete session: {}", e);
        }
    }
    session.purge();
}

/// Ends every session of `user_id`, as after a password change.
pub fn logout_everywhere(conn: &mut DbConnection, user_id: i32) -> QueryResult<usize> {
    diesel::delete(user_sessions::table.filter(user_sessions::user_id.eq(user_id))).execute(conn)
}

/// Per-request bookkeeping for logged-in visitors, like upstream's `before_request`: keeps
/// the session alive and records the visitor's current IP as `last_login_ip`, which IP
/// bans go by. Writes only when something changed.
pub fn touch_session(conn: &mut DbConnection, sid: &str, ip: Option<Vec<u8>>) -> QueryResult<()> {
    let row = user_sessions::table
        .inner_join(users::table)
        .filter(user_sessions::id.eq(sid))
        .filter(user_sessions::last_seen.gt(expiry_cutoff()))
        .select((user_sessions::user_id, user_sessions::last_seen, user_sessions::ip, users::last_login_ip))
        .first::<(i32, NaiveDateTime, Option<Vec<u8>>, Option<Vec<u8>>)>(conn)
        .optional()?;
    let Some((user_id, last_seen, session_ip, user_ip)) = row else {
        return Ok(());
    };

    let now = now();
    if now - last_seen > Duration::minutes(LAST_SEEN_REFRESH_MINUTES) || session_ip != ip {
        diesel::update(user_sessions::table.find(sid))
            .set((user_sessions::last_seen.eq(now), user_sessions::ip.eq(&ip)))
            .execute(conn)?;
    }
    if ip.is_some() && user_ip != ip {
        diesel::update(users::table.find(user_id)).set(users::last_login_ip.eq(&ip)).execute(conn)?;
    }
    Ok(())
}

/// Middleware that runs [`touch_session`] for requests carrying a session. It must sit
/// inside the session middleware (be registered before it) to see the session.
pub async fn refresh_session(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let sid = req.get_session().get::<String>(SESSION_ID_KEY).ok().flatten();
    let pool = req.app_data::<web::Data<DbPool>>().cloned();
    if let (Some(sid), Some(pool)) = (sid, pool) {
        let ip = crate::utils::client_ip(req.request());
        let touched = web::block(move || -> anyhow::Result<()> {
            touch_session(&mut *pool.get()?, &sid, ip)?;
            Ok(())
        })
        .await;
        if let Ok(Err(e)) = touched {
            log::error!("Failed to update session: {:#}", e);
        }
    }
    next.call(req).await
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// Test route body: signs in as the user in the path, as a real login would.
    pub async fn login(session: Session, pool: web::Data<DbPool>, path: web::Path<i32>) -> actix_web::HttpResponse {
        login_user(&session, &mut pool.get().unwrap(), path.into_inner(), None, AuthMethod::Password).unwrap();
        actix_web::HttpResponse::Ok().finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> DbConnection {
        let mut conn = crate::db::connect(":memory:").unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query("INSERT INTO users (id, username, password_hash, status) VALUES (1, 'u', 'x', 1)")
            .execute(&mut conn)
            .unwrap();
        conn
    }

    fn add_session(conn: &mut DbConnection, sid: &str, last_seen: NaiveDateTime) {
        diesel::insert_into(user_sessions::table)
            .values((
                user_sessions::id.eq(sid),
                user_sessions::user_id.eq(1),
                user_sessions::created_time.eq(last_seen),
                user_sessions::last_seen.eq(last_seen),
            ))
            .execute(conn)
            .unwrap();
    }

    #[test]
    fn sessions_expire_and_can_be_deleted() {
        let mut conn = db();
        add_session(&mut conn, "live", now());
        add_session(&mut conn, "old", now() - Duration::days(SESSION_TTL_DAYS + 1));

        assert_eq!(user_by_session(&mut conn, "live").unwrap().map(|u| u.id), Some(1));
        assert!(user_by_session(&mut conn, "old").unwrap().is_none());
        assert!(user_by_session(&mut conn, "unknown").unwrap().is_none());

        assert_eq!(logout_everywhere(&mut conn, 1).unwrap(), 2);
        assert!(user_by_session(&mut conn, "live").unwrap().is_none());
    }

    #[test]
    fn banned_users_have_no_session() {
        let mut conn = db();
        add_session(&mut conn, "s", now());
        diesel::sql_query("UPDATE users SET status = 2").execute(&mut conn).unwrap();
        assert!(user_by_session(&mut conn, "s").unwrap().is_none());
    }

    #[test]
    fn touch_records_ip_and_refreshes_last_seen() {
        let mut conn = db();
        let stale = now() - Duration::hours(1);
        add_session(&mut conn, "s", stale);
        let ip = Some(crate::utils::pack_ip("10.0.0.1".parse().unwrap()));

        touch_session(&mut conn, "s", ip.clone()).unwrap();

        let (seen, session_ip): (NaiveDateTime, Option<Vec<u8>>) = user_sessions::table
            .find("s")
            .select((user_sessions::last_seen, user_sessions::ip))
            .first(&mut conn)
            .unwrap();
        assert!(seen > stale);
        assert_eq!(session_ip, ip);
        let user_ip: Option<Vec<u8>> = users::table.find(1).select(users::last_login_ip).first(&mut conn).unwrap();
        assert_eq!(user_ip, ip);
    }
}
