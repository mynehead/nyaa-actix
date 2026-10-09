use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db::schema::{user_preferences, users};
use crate::db::DbConnection;

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserStatus {
    Inactive = 0,
    Active = 1,
    Banned = 2,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UserLevel {
    Regular = 0,
    Trusted = 1,
    Moderator = 2,
    SuperAdmin = 3,
}

impl UserLevel {
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => UserLevel::Trusted,
            2 => UserLevel::Moderator,
            3 => UserLevel::SuperAdmin,
            _ => UserLevel::Regular,
        }
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = users)]
pub struct User {
    pub id: i32,
    pub username: String,
    pub email: Option<String>,
    // The hash and IPs never go into template context (users are serialized for Tera);
    // pages that show the IPs pass them on their own.
    #[serde(skip_serializing, default)]
    pub password_hash: String,
    pub status: i32,
    pub level: i32,
    pub created_time: NaiveDateTime,
    pub last_login_date: Option<NaiveDateTime>,
    #[serde(skip_serializing, default)]
    pub last_login_ip: Option<Vec<u8>>,
    #[serde(skip_serializing, default)]
    pub registration_ip: Option<Vec<u8>>,
    /// When the current uploaded avatar was set; None when there is none.
    pub avatar_time: Option<NaiveDateTime>,
}

/// Argon2 hash with a fresh salt, as stored in `password_hash`.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(password.as_bytes(), &salt).expect("failed to hash password").to_string()
}

/// Checks `password` against `user`'s hash. Without a user it checks a throwaway hash
/// anyway, so an unknown username takes as long to reject as a wrong password.
pub fn password_matches(user: Option<&User>, password: &str) -> bool {
    static DUMMY_HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| hash_password("no such user"));
    match user {
        Some(u) => u.verify_password(password),
        None => {
            let _ = verify_hash(&DUMMY_HASH, password);
            false
        }
    }
}

/// Only Argon2 hashes count, so a placeholder hash (as an account without a local password
/// would get) can never be matched.
fn verify_hash(hash: &str, password: &str) -> bool {
    hash.starts_with("$argon2")
        && PasswordHash::new(hash)
            .is_ok_and(|hash| Argon2::default().verify_password(password.as_bytes(), &hash).is_ok())
}

pub const DEFAULT_AVATAR: &str = "/static/img/avatar/default.png";

impl User {
    pub fn is_active(&self) -> bool {
        self.status == UserStatus::Active as i32
    }

    pub fn is_banned(&self) -> bool {
        self.status == UserStatus::Banned as i32
    }

    // What a level may do is in `crate::auth`: `User::level` and `User::can`.

    /// Seconds since the account was created (upstream `User.age`).
    pub fn age_secs(&self) -> i64 {
        (chrono::Utc::now().naive_utc() - self.created_time).num_seconds()
    }

    pub fn level_str(&self) -> String {
        let level = match UserLevel::from_i32(self.level) {
            UserLevel::Regular => "User",
            UserLevel::Trusted => "Trusted",
            UserLevel::Moderator => "Moderator",
            UserLevel::SuperAdmin => "Administrator",
        };
        if self.is_banned() {
            format!("BANNED {}", level)
        } else {
            level.to_string()
        }
    }

    pub fn status_str(&self) -> &'static str {
        match self.status {
            1 => "Active",
            2 => "Banned",
            _ => "Inactive",
        }
    }

    /// Bootstrap text color suffix; banned users are also struck through, as upstream.
    pub fn level_color(&self) -> String {
        let color = match UserLevel::from_i32(self.level) {
            UserLevel::Regular => "default",
            UserLevel::Trusted => "success",
            UserLevel::Moderator | UserLevel::SuperAdmin => "purple",
        };
        if self.is_banned() {
            format!("{} strike", color)
        } else {
            color.to_string()
        }
    }

    pub fn verify_password(&self, password: &str) -> bool {
        verify_hash(&self.password_hash, password)
    }

    /// The uploaded avatar, else Gravatar (or the GRAVATAR_URL service) when enabled (upstream
    /// `gravatar_url`), else the default.
    pub fn avatar_url(&self, cfg: &Config) -> String {
        if let Some(t) = self.avatar_time {
            return format!("/avatar/{}?v={}", self.id, t.and_utc().timestamp());
        }
        match &self.email {
            Some(email) if cfg.enable_gravatar => {
                // Only the hash leaves the site, never the address itself.
                let email = email.trim().to_lowercase();
                let hash = if cfg.gravatar_sha256 {
                    use sha2::{Digest, Sha256};
                    hex::encode(Sha256::digest(email.as_bytes()))
                } else {
                    use md5::{Digest, Md5};
                    hex::encode(Md5::digest(email.as_bytes()))
                };
                let default_url = format!("{}{}", cfg.site_url, DEFAULT_AVATAR);
                // Nyaa: PG-rated, Sukebei: X-rated
                let rating = if cfg.site_flavor == "nyaa" { "pg" } else { "x" };
                format!("{}/{}?s=120&d={}&r={}", cfg.gravatar_url, hash, urlencoding::encode(&default_url), rating)
            }
            _ => DEFAULT_AVATAR.to_string(),
        }
    }

    pub fn set_password(conn: &mut DbConnection, uid: i32, password: &str) -> QueryResult<usize> {
        diesel::update(users::table.find(uid)).set(users::password_hash.eq(hash_password(password))).execute(conn)
    }

    pub fn set_email(conn: &mut DbConnection, uid: i32, email: &str) -> QueryResult<usize> {
        diesel::update(users::table.find(uid)).set(users::email.eq(email)).execute(conn)
    }

    pub fn set_avatar_time(conn: &mut DbConnection, uid: i32, time: NaiveDateTime) -> QueryResult<usize> {
        diesel::update(users::table.find(uid)).set(users::avatar_time.eq(time)).execute(conn)
    }

    /// The "Hide comments by default" preference; off when the user never saved preferences.
    pub fn hide_comments(conn: &mut DbConnection, uid: i32) -> QueryResult<bool> {
        let hide: Option<i32> =
            user_preferences::table.find(uid).select(user_preferences::hide_comments).first(conn).optional()?;
        Ok(hide.unwrap_or(0) != 0)
    }

    pub fn set_hide_comments(conn: &mut DbConnection, uid: i32, hide: bool) -> QueryResult<usize> {
        // Update, else insert: REPLACE INTO only exists on SQLite
        conn.transaction(|conn| {
            let updated = diesel::update(user_preferences::table.find(uid))
                .set(user_preferences::hide_comments.eq(hide as i32))
                .execute(conn)?;
            if updated > 0 {
                return Ok(updated);
            }
            diesel::insert_into(user_preferences::table)
                .values((user_preferences::user_id.eq(uid), user_preferences::hide_comments.eq(hide as i32)))
                .execute(conn)
        })
    }

    pub fn by_id(conn: &mut DbConnection, uid: i32) -> QueryResult<Option<User>> {
        users::table.find(uid).first(conn).optional()
    }

    pub fn by_username(conn: &mut DbConnection, name: &str) -> QueryResult<Option<User>> {
        users::table.filter(users::username.eq(name)).first(conn).optional()
    }

    /// The user with this name, ignoring case.
    pub fn by_username_ignoring_case(conn: &mut DbConnection, name: &str) -> QueryResult<Option<User>> {
        use crate::search::db::lower;
        users::table.filter(lower(users::username).eq(name.to_lowercase())).first(conn).optional()
    }

    /// Whether a user with this name exists, ignoring case, so nobody can register a
    /// look-alike of an existing name ("Admin" next to "admin").
    pub fn username_taken(conn: &mut DbConnection, name: &str) -> QueryResult<bool> {
        use crate::search::db::lower;
        let n: i64 = users::table.filter(lower(users::username).eq(name.to_lowercase())).count().get_result(conn)?;
        Ok(n > 0)
    }

    pub fn by_email(conn: &mut DbConnection, addr: &str) -> QueryResult<Option<User>> {
        users::table.filter(users::email.eq(addr)).first(conn).optional()
    }

    pub fn by_username_or_email(conn: &mut DbConnection, val: &str) -> QueryResult<Option<User>> {
        let by_name = users::table.filter(users::username.eq(val)).first(conn).optional()?;
        if by_name.is_some() {
            return Ok(by_name);
        }
        users::table.filter(users::email.eq(val)).first(conn).optional()
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = users)]
pub struct NewUser {
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub status: i32,
    pub level: i32,
    pub created_time: NaiveDateTime,
    pub registration_ip: Option<Vec<u8>>,
}

impl NewUser {
    pub fn new(username: &str, email: Option<&str>, password: &str) -> Self {
        let hash = hash_password(password);
        NewUser {
            username: username.to_string(),
            email: email.map(|e| e.to_string()),
            password_hash: hash,
            status: UserStatus::Active as i32,
            level: UserLevel::Regular as i32,
            created_time: chrono::Utc::now().naive_utc(),
            registration_ip: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_json_leaves_out_hash_and_ips() {
        let user = User {
            id: 1,
            username: "alice".into(),
            email: Some("a@example.com".into()),
            password_hash: hash_password("secret123"),
            status: UserStatus::Active as i32,
            level: UserLevel::Regular as i32,
            created_time: chrono::Utc::now().naive_utc(),
            last_login_date: None,
            last_login_ip: Some(vec![127, 0, 0, 1]),
            registration_ip: Some(vec![127, 0, 0, 1]),
            avatar_time: None,
        };
        let json = serde_json::to_value(&user).unwrap();
        for hidden in ["password_hash", "last_login_ip", "registration_ip"] {
            assert!(json.get(hidden).is_none(), "{hidden} serialized");
        }
        // Tera filters read users back from that JSON
        let back: User = serde_json::from_value(json).unwrap();
        assert_eq!(back.username, "alice");
    }

    #[test]
    fn password_matches_checks_the_hash_and_rejects_missing_users() {
        let mut user: User = serde_json::from_value(serde_json::json!({
            "id": 1, "username": "alice", "email": null, "status": 1, "level": 0,
            "created_time": "2026-10-06T00:00:00", "last_login_date": null, "avatar_time": null
        }))
        .unwrap();
        user.password_hash = hash_password("secret123");
        assert!(password_matches(Some(&user), "secret123"));
        assert!(!password_matches(Some(&user), "wrong"));
        assert!(!password_matches(None, "secret123"));
    }
}
