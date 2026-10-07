use crate::db::schema::{bans, users};
use crate::db::DbConnection;
use crate::utils::unpack_ip;
use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

/// A row of the `bans` table: a user ban, an IP ban, or both. Listed on /admin/bans and
/// managed from the user page, as upstream.
#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = bans)]
pub struct Ban {
    pub id: i32,
    pub created_time: NaiveDateTime,
    pub admin_id: i32,
    pub user_id: Option<i32>,
    pub user_ip: Option<Vec<u8>>,
    pub reason: String,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = bans)]
pub struct NewBan {
    pub created_time: NaiveDateTime,
    pub admin_id: i32,
    pub user_id: Option<i32>,
    pub user_ip: Option<Vec<u8>>,
    pub reason: String,
}

/// A ban with the names the ban lists show.
#[derive(Debug, Clone, Serialize)]
pub struct BanEntry {
    #[serde(flatten)]
    pub ban: Ban,
    pub admin_name: String,
    pub user_name: Option<String>,
    /// The banned IP as text, as upstream's `ip_string`.
    pub ip_string: Option<String>,
}

/// Upstream caps ban reasons at 1024 characters on the form.
pub const MAX_BAN_REASON_LEN: usize = 1024;

impl Ban {
    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<Ban>> {
        bans::table.find(id).select(Ban::as_select()).first(conn).optional()
    }

    /// Bans on this user or this IP (upstream `Ban.banned`); empty when both are `None`.
    pub fn banned(conn: &mut DbConnection, user_id: Option<i32>, user_ip: Option<&[u8]>) -> QueryResult<Vec<Ban>> {
        let mut query = bans::table.select(Ban::as_select()).order(bans::id).into_boxed();
        match (user_id, user_ip) {
            (Some(uid), Some(ip)) => query = query.filter(bans::user_id.eq(uid).or(bans::user_ip.eq(ip.to_vec()))),
            (Some(uid), None) => query = query.filter(bans::user_id.eq(uid)),
            (None, Some(ip)) => query = query.filter(bans::user_ip.eq(ip.to_vec())),
            (None, None) => return Ok(vec![]),
        }
        query.load(conn)
    }

    /// Whether any ban covers this IP.
    pub fn ip_banned(conn: &mut DbConnection, ip: &[u8]) -> QueryResult<bool> {
        diesel::select(diesel::dsl::exists(bans::table.filter(bans::user_ip.eq(ip.to_vec())))).get_result(conn)
    }

    pub fn ip_string(&self) -> Option<String> {
        self.user_ip.as_deref().and_then(unpack_ip).map(|ip| ip.to_string())
    }

    /// Adds the moderator's and banned user's names to each ban.
    pub fn with_names(conn: &mut DbConnection, bans: Vec<Ban>) -> QueryResult<Vec<BanEntry>> {
        let mut ids: Vec<i32> = bans.iter().flat_map(|b| [Some(b.admin_id), b.user_id]).flatten().collect();
        ids.sort_unstable();
        ids.dedup();
        let names: std::collections::HashMap<i32, String> = users::table
            .filter(users::id.eq_any(ids))
            .select((users::id, users::username))
            .load::<(i32, String)>(conn)?
            .into_iter()
            .collect();
        Ok(bans
            .into_iter()
            .map(|ban| BanEntry {
                admin_name: names.get(&ban.admin_id).cloned().unwrap_or_default(),
                user_name: ban.user_id.and_then(|id| names.get(&id).cloned()),
                ip_string: ban.ip_string(),
                ban,
            })
            .collect())
    }

    /// One page of bans, newest first, and the total number of bans.
    pub fn page(conn: &mut DbConnection, page: i64, per_page: i64) -> QueryResult<(Vec<BanEntry>, i64)> {
        let total = bans::table.count().get_result(conn)?;
        let rows = bans::table
            .select(Ban::as_select())
            .order((bans::created_time.desc(), bans::id.desc()))
            .limit(per_page)
            .offset((page.max(1) - 1) * per_page)
            .load(conn)?;
        Ok((Ban::with_names(conn, rows)?, total))
    }
}
