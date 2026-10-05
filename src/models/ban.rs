use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use crate::db::schema::bans;

/// A row of the `bans` table (user and/or IP). Read by the planned /admin/bans page and by
/// ban checks on login and upload (roadmap steps 3 and 4).
#[allow(dead_code)]
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
