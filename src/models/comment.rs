use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};
use crate::db::schema::nyaa_comments;

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = nyaa_comments)]
pub struct Comment {
    pub id: i32,
    pub torrent_id: i32,
    pub user_id: Option<i32>,
    pub created_time: NaiveDateTime,
    pub edited_time: Option<NaiveDateTime>,
    pub text: String,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = nyaa_comments)]
pub struct NewComment {
    pub torrent_id: i32,
    pub user_id: Option<i32>,
    pub created_time: NaiveDateTime,
    pub text: String,
}
