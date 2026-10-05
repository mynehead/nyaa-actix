use actix_session::Session;
use chrono::{Duration, NaiveDateTime, Utc};
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::SqliteConnection;
use rand::RngCore;

use crate::db::schema::{user_sessions, users};
use crate::models::User;

/// The cookie only carries this random id; the `user_sessions` row is the real session,
/// so deleting it logs the browser out even if the cookie was copied.
pub const SESSION_ID_KEY: &str = "sid";

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

pub fn get_current_user(session: &Session, pool: &Pool<ConnectionManager<SqliteConnection>>) -> Option<User> {
    let sid: String = session.get(SESSION_ID_KEY).ok()??;
    let mut conn = pool.get().ok()?;
    user_by_session(&mut conn, &sid).ok()?
}

/// The active user behind an unexpired session. Banned or deactivated users lose access
/// immediately, not at next login.
fn user_by_session(conn: &mut SqliteConnection, sid: &str) -> QueryResult<Option<User>> {
    let user = user_sessions::table
        .inner_join(users::table)
        .filter(user_sessions::id.eq(sid))
        .filter(user_sessions::last_seen.gt(expiry_cutoff()))
        .select(User::as_select())
        .first::<User>(conn)
        .optional()?;
    Ok(user.filter(|u| u.is_active()))
}

/// Starts a fresh session (new cookie, new server-side row) and records the login like
/// upstream: `last_login_date` and `last_login_ip`.
pub fn login_user(
    session: &Session,
    conn: &mut SqliteConnection,
    user_id: i32,
    ip: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let sid = hex::encode(bytes);
    let now = now();

    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        // Housekeeping: expired rows are never valid again
        diesel::delete(user_sessions::table.filter(user_sessions::last_seen.le(expiry_cutoff())))
            .execute(conn)?;
        diesel::insert_into(user_sessions::table)
            .values((
                user_sessions::id.eq(&sid),
                user_sessions::user_id.eq(user_id),
                user_sessions::created_time.eq(now),
                user_sessions::last_seen.eq(now),
                user_sessions::ip.eq(&ip),
            ))
            .execute(conn)?;
        diesel::update(users::table.find(user_id))
            .set((users::last_login_date.eq(Some(now)), users::last_login_ip.eq(&ip)))
            .execute(conn)?;
        Ok(())
    })?;

    // New cookie on login, so a session id planted before login is useless
    session.renew();
    session.insert(SESSION_ID_KEY, sid)?;
    Ok(())
}

pub fn logout_user(session: &Session, pool: &Pool<ConnectionManager<SqliteConnection>>) {
    if let (Ok(Some(sid)), Ok(mut conn)) = (session.get::<String>(SESSION_ID_KEY), pool.get()) {
        if let Err(e) = diesel::delete(user_sessions::table.find(sid)).execute(&mut conn) {
            log::error!("Failed to delete session: {}", e);
        }
    }
    session.purge();
}

/// Per-request bookkeeping for logged-in visitors, like upstream's `before_request`:
/// keeps the session alive and records the visitor's current IP as `last_login_ip`,
/// which IP bans and rate limits go by. Writes only when something changed.
pub fn touch_session(conn: &mut SqliteConnection, sid: &str, ip: Option<Vec<u8>>) -> QueryResult<()> {
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
        diesel::update(users::table.find(user_id))
            .set(users::last_login_ip.eq(&ip))
            .execute(conn)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel_migrations::MigrationHarness;

    fn db() -> SqliteConnection {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.run_pending_migrations(crate::MIGRATIONS).unwrap();
        diesel::sql_query("INSERT INTO users (id, username, password_hash, status) VALUES (1, 'u', 'x', 1)")
            .execute(&mut conn).unwrap();
        conn
    }

    fn add_session(conn: &mut SqliteConnection, sid: &str, last_seen: NaiveDateTime) {
        diesel::insert_into(user_sessions::table)
            .values((
                user_sessions::id.eq(sid),
                user_sessions::user_id.eq(1),
                user_sessions::created_time.eq(last_seen),
                user_sessions::last_seen.eq(last_seen),
            ))
            .execute(conn).unwrap();
    }

    #[test]
    fn sessions_expire_and_can_be_deleted() {
        let mut conn = db();
        add_session(&mut conn, "live", now());
        add_session(&mut conn, "old", now() - Duration::days(SESSION_TTL_DAYS + 1));

        assert_eq!(user_by_session(&mut conn, "live").unwrap().map(|u| u.id), Some(1));
        assert!(user_by_session(&mut conn, "old").unwrap().is_none());
        assert!(user_by_session(&mut conn, "unknown").unwrap().is_none());

        diesel::delete(user_sessions::table.find("live")).execute(&mut conn).unwrap();
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

        let (seen, session_ip): (NaiveDateTime, Option<Vec<u8>>) = user_sessions::table.find("s")
            .select((user_sessions::last_seen, user_sessions::ip)).first(&mut conn).unwrap();
        assert!(seen > stale);
        assert_eq!(session_ip, ip);
        let user_ip: Option<Vec<u8>> = users::table.find(1).select(users::last_login_ip).first(&mut conn).unwrap();
        assert_eq!(user_ip, ip);
    }
}
