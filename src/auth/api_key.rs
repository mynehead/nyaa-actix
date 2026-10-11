//! Personal API keys: what scripts send to `/api/` in place of the password, so accounts
//! with two-factor sign-in can use the API without it. A key is shown once when it is
//! made and stored as SHA-256 (it carries 192 random bits, so no slow hash is needed).
//! Revoking deletes the row.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::schema::user_api_keys;
use crate::db::DbConnection;

/// How many keys one account may have.
pub const MAX_KEYS: i64 = 10;
pub const MAX_NAME_LEN: usize = 64;
/// Every key starts with this, so the API can tell a key from a password.
pub const PREFIX: &str = "nyaa_";

pub fn new_key() -> String {
    let mut bytes = [0u8; 24];
    OsRng.fill_bytes(&mut bytes);
    format!("{PREFIX}{}", hex::encode(bytes))
}

pub fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.trim().as_bytes()))
}

pub fn looks_like_key(s: &str) -> bool {
    s.trim().starts_with(PREFIX)
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = user_api_keys)]
pub struct ApiKey {
    pub id: i32,
    pub name: String,
    pub created_time: NaiveDateTime,
    pub last_used_time: Option<NaiveDateTime>,
}

impl ApiKey {
    pub fn list(conn: &mut DbConnection, user_id: i32) -> QueryResult<Vec<ApiKey>> {
        user_api_keys::table
            .filter(user_api_keys::user_id.eq(user_id))
            .order(user_api_keys::id)
            .select(ApiKey::as_select())
            .load(conn)
    }

    /// Makes a key named `name`; the plain key to show once, or None when the account
    /// already has [`MAX_KEYS`].
    pub fn create(conn: &mut DbConnection, user_id: i32, name: &str) -> QueryResult<Option<String>> {
        conn.transaction(|conn| {
            let count: i64 =
                user_api_keys::table.filter(user_api_keys::user_id.eq(user_id)).count().get_result(conn)?;
            if count >= MAX_KEYS {
                return Ok(None);
            }
            let key = new_key();
            diesel::insert_into(user_api_keys::table)
                .values((
                    user_api_keys::user_id.eq(user_id),
                    user_api_keys::name.eq(name),
                    user_api_keys::key_hash.eq(hash_key(&key)),
                    user_api_keys::created_time.eq(chrono::Utc::now().naive_utc()),
                ))
                .execute(conn)?;
            Ok(Some(key))
        })
    }

    /// Deletes one of the user's keys; whether it existed.
    pub fn revoke(conn: &mut DbConnection, user_id: i32, id: i32) -> QueryResult<bool> {
        let n =
            diesel::delete(user_api_keys::table.find(id).filter(user_api_keys::user_id.eq(user_id))).execute(conn)?;
        Ok(n > 0)
    }

    /// Whether `key` is one of the user's keys; records the use when it is.
    pub fn check(conn: &mut DbConnection, user_id: i32, key: &str) -> QueryResult<bool> {
        let n = diesel::update(
            user_api_keys::table
                .filter(user_api_keys::key_hash.eq(hash_key(key)))
                .filter(user_api_keys::user_id.eq(user_id)),
        )
        .set(user_api_keys::last_used_time.eq(Some(chrono::Utc::now().naive_utc())))
        .execute(conn)?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_work_for_their_owner_until_revoked() {
        let mut conn = crate::db::connect(&crate::db::test_url()).unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status) VALUES (1, 'a', 'x', 1), (2, 'b', 'x', 1)",
        )
        .execute(&mut conn)
        .unwrap();

        let key = ApiKey::create(&mut conn, 1, "script").unwrap().unwrap();
        assert!(looks_like_key(&key) && key.len() == PREFIX.len() + 48);
        assert!(ApiKey::check(&mut conn, 1, &key).unwrap());
        assert!(!ApiKey::check(&mut conn, 2, &key).unwrap(), "someone else's key");
        assert!(!ApiKey::check(&mut conn, 1, &new_key()).unwrap());

        let keys = ApiKey::list(&mut conn, 1).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].last_used_time.is_some());
        assert!(!ApiKey::revoke(&mut conn, 2, keys[0].id).unwrap(), "not b's key");
        assert!(ApiKey::revoke(&mut conn, 1, keys[0].id).unwrap());
        assert!(!ApiKey::check(&mut conn, 1, &key).unwrap(), "revoked");

        for _ in 0..MAX_KEYS {
            assert!(ApiKey::create(&mut conn, 2, "k").unwrap().is_some());
        }
        assert!(ApiKey::create(&mut conn, 2, "k").unwrap().is_none(), "limit");
    }
}
