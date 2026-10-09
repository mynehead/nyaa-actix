use crate::db::schema::{ip_range_bans, users};
use crate::db::DbConnection;
use crate::utils::proxy::IpNet;
use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;

/// A banned network: nobody connecting from it can use the site (see
/// `middleware::ip_range_ban`). Listed and managed on /admin/bans.
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = ip_range_bans)]
pub struct IpRangeBan {
    pub id: i32,
    /// Canonical CIDR text, as `IpNet`'s `Display` writes it.
    pub cidr: String,
    pub reason: String,
    pub created_time: NaiveDateTime,
    /// `None`: until lifted.
    pub expires_time: Option<NaiveDateTime>,
    pub admin_id: i32,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = ip_range_bans)]
pub struct NewIpRangeBan {
    pub cidr: String,
    pub reason: String,
    pub created_time: NaiveDateTime,
    pub expires_time: Option<NaiveDateTime>,
    pub admin_id: i32,
}

/// A range ban with what /admin/bans shows next to it.
#[derive(Debug, Clone, Serialize)]
pub struct IpRangeBanEntry {
    #[serde(flatten)]
    pub ban: IpRangeBan,
    pub admin_name: String,
    pub expired: bool,
}

impl IpRangeBan {
    pub fn net(&self) -> Option<IpNet> {
        IpNet::parse(&self.cidr)
    }

    pub fn is_expired(&self, now: NaiveDateTime) -> bool {
        self.expires_time.is_some_and(|t| t <= now)
    }

    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<IpRangeBan>> {
        ip_range_bans::table.find(id).select(IpRangeBan::as_select()).first(conn).optional()
    }

    pub fn by_cidr(conn: &mut DbConnection, cidr: &str) -> QueryResult<Option<IpRangeBan>> {
        ip_range_bans::table.filter(ip_range_bans::cidr.eq(cidr)).select(IpRangeBan::as_select()).first(conn).optional()
    }

    /// Every range ban, expired ones included.
    pub fn all(conn: &mut DbConnection) -> QueryResult<Vec<IpRangeBan>> {
        ip_range_bans::table.select(IpRangeBan::as_select()).order(ip_range_bans::id).load(conn)
    }

    /// All range bans for the admin page, newest first, with the moderator's name.
    pub fn list(conn: &mut DbConnection, now: NaiveDateTime) -> QueryResult<Vec<IpRangeBanEntry>> {
        let rows: Vec<(IpRangeBan, Option<String>)> = ip_range_bans::table
            .left_join(users::table)
            .select((IpRangeBan::as_select(), users::username.nullable()))
            .order((ip_range_bans::created_time.desc(), ip_range_bans::id.desc()))
            .load(conn)?;
        Ok(rows
            .into_iter()
            .map(|(ban, admin_name)| IpRangeBanEntry {
                expired: ban.is_expired(now),
                admin_name: admin_name.unwrap_or_default(),
                ban,
            })
            .collect())
    }

    pub fn insert(conn: &mut DbConnection, ban: &NewIpRangeBan) -> QueryResult<usize> {
        diesel::insert_into(ip_range_bans::table).values(ban).execute(conn)
    }

    pub fn delete(conn: &mut DbConnection, id: i32) -> QueryResult<usize> {
        diesel::delete(ip_range_bans::table.find(id)).execute(conn)
    }
}
