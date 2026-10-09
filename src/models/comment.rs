use crate::db::schema::nyaa_comments;
use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

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

impl Comment {
    /// When upstream's EDITING_TIME_LIMIT (`limit` seconds, 0 = none) runs out for this comment.
    pub fn editable_until(&self, limit: i64) -> Option<NaiveDateTime> {
        (limit > 0).then(|| self.created_time + chrono::Duration::seconds(limit))
    }

    /// Upstream's `editing_limit_exceeded`: the author may no longer edit or delete it.
    pub fn editing_limit_exceeded(&self, limit: i64) -> bool {
        self.editable_until(limit).is_some_and(|until| chrono::Utc::now().naive_utc() >= until)
    }
}
