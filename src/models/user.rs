use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{rand_core::OsRng, SaltString};

use crate::db::schema::users;

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
    pub password_hash: String,
    pub status: i32,
    pub level: i32,
    pub created_time: NaiveDateTime,
    pub last_login_date: Option<NaiveDateTime>,
    pub last_login_ip: Option<Vec<u8>>,
    pub registration_ip: Option<Vec<u8>>,
}

impl User {
    pub fn is_active(&self) -> bool {
        self.status == UserStatus::Active as i32
    }

    pub fn is_banned(&self) -> bool {
        self.status == UserStatus::Banned as i32
    }

    pub fn is_trusted(&self) -> bool {
        self.level >= UserLevel::Trusted as i32
    }

    pub fn is_moderator(&self) -> bool {
        self.level >= UserLevel::Moderator as i32
    }

    /// Admin-only actions upstream, such as changing a user's level or nuking a user (roadmap step 4).
    #[allow(dead_code)]
    pub fn is_superadmin(&self) -> bool {
        self.level == UserLevel::SuperAdmin as i32
    }

    pub fn level_str(&self) -> String {
        let level = match UserLevel::from_i32(self.level) {
            UserLevel::Regular => "User",
            UserLevel::Trusted => "Trusted",
            UserLevel::Moderator => "Moderator",
            UserLevel::SuperAdmin => "Administrator",
        };
        if self.is_banned() { format!("BANNED {}", level) } else { level.to_string() }
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
        if self.is_banned() { format!("{} strike", color) } else { color.to_string() }
    }

    pub fn verify_password(&self, password: &str) -> bool {
        if let Ok(hash) = PasswordHash::new(&self.password_hash) {
            Argon2::default().verify_password(password.as_bytes(), &hash).is_ok()
        } else {
            false
        }
    }

    pub fn by_id(conn: &mut SqliteConnection, uid: i32) -> QueryResult<Option<User>> {
        users::table.find(uid).first(conn).optional()
    }

    pub fn by_username(conn: &mut SqliteConnection, name: &str) -> QueryResult<Option<User>> {
        users::table.filter(users::username.eq(name)).first(conn).optional()
    }

    pub fn by_email(conn: &mut SqliteConnection, addr: &str) -> QueryResult<Option<User>> {
        users::table.filter(users::email.eq(addr)).first(conn).optional()
    }

    pub fn by_username_or_email(conn: &mut SqliteConnection, val: &str) -> QueryResult<Option<User>> {
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
}

impl NewUser {
    pub fn new(username: &str, email: Option<&str>, password: &str) -> Self {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("failed to hash password")
            .to_string();
        NewUser {
            username: username.to_string(),
            email: email.map(|e| e.to_string()),
            password_hash: hash,
            status: UserStatus::Active as i32,
            level: UserLevel::Regular as i32,
            created_time: chrono::Utc::now().naive_utc(),
        }
    }
}
