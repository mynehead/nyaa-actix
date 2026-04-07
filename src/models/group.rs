use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::{groups, group_members};

pub const PERM_UPLOAD: i32 = 1;
pub const PERM_EDIT: i32 = 2;

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = groups)]
pub struct Group {
    pub id: i32,
    pub name: String,
    pub tag: String,
    pub slug: String,
    pub description: Option<String>,
    pub created_time: NaiveDateTime,
    pub owner_id: i32,
}

impl Group {
    pub fn by_id(conn: &mut SqliteConnection, gid: i32) -> QueryResult<Option<Group>> {
        groups::table.find(gid).first(conn).optional()
    }

    pub fn by_slug(conn: &mut SqliteConnection, slug: &str) -> QueryResult<Option<Group>> {
        groups::table.filter(groups::slug.eq(slug)).first(conn).optional()
    }

    pub fn all(conn: &mut SqliteConnection) -> QueryResult<Vec<Group>> {
        groups::table.order(groups::name.asc()).load(conn)
    }

    pub fn member_permissions(&self, conn: &mut SqliteConnection, user_id: i32) -> QueryResult<i32> {
        if user_id == self.owner_id {
            return Ok(PERM_UPLOAD | PERM_EDIT);
        }
        let row = group_members::table
            .filter(group_members::group_id.eq(self.id))
            .filter(group_members::user_id.eq(user_id))
            .select(group_members::permissions)
            .first::<i32>(conn)
            .optional()?;
        Ok(row.unwrap_or(0))
    }

    pub fn can_upload(&self, conn: &mut SqliteConnection, user_id: i32) -> bool {
        self.member_permissions(conn, user_id)
            .map(|p| p & PERM_UPLOAD != 0)
            .unwrap_or(false)
    }

    pub fn can_edit(&self, conn: &mut SqliteConnection, user_id: i32) -> bool {
        self.member_permissions(conn, user_id)
            .map(|p| p & PERM_EDIT != 0)
            .unwrap_or(false)
    }

    pub fn member_count(&self, conn: &mut SqliteConnection) -> i64 {
        group_members::table
            .filter(group_members::group_id.eq(self.id))
            .count()
            .get_result(conn)
            .unwrap_or(0)
    }

    pub fn members_with_perms(&self, conn: &mut SqliteConnection) -> QueryResult<Vec<(i32, i32)>> {
        group_members::table
            .filter(group_members::group_id.eq(self.id))
            .select((group_members::user_id, group_members::permissions))
            .load(conn)
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = groups)]
pub struct NewGroup {
    pub name: String,
    pub tag: String,
    pub slug: String,
    pub description: Option<String>,
    pub created_time: NaiveDateTime,
    pub owner_id: i32,
}
