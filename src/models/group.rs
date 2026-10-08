use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::{group_members, groups};
use crate::db::DbConnection;

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
    pub fn by_id(conn: &mut DbConnection, gid: i32) -> QueryResult<Option<Group>> {
        groups::table.find(gid).first(conn).optional()
    }

    pub fn by_slug(conn: &mut DbConnection, slug: &str) -> QueryResult<Option<Group>> {
        groups::table.filter(groups::slug.eq(slug)).first(conn).optional()
    }

    pub fn by_ids(conn: &mut DbConnection, ids: &[i32]) -> QueryResult<Vec<Group>> {
        groups::table.filter(groups::id.eq_any(ids)).load(conn)
    }

    /// A torrent name without a leading "[tag]", which the page shows as the
    /// group's link instead (so names that already carry the tag don't show it twice).
    pub fn strip_tag<'a>(&self, name: &'a str) -> &'a str {
        let prefix = format!("[{}]", self.tag);
        match name.get(..prefix.len()) {
            Some(head) if head.eq_ignore_ascii_case(&prefix) && !name[prefix.len()..].trim().is_empty() => {
                name[prefix.len()..].trim_start()
            }
            _ => name,
        }
    }

    pub fn all(conn: &mut DbConnection) -> QueryResult<Vec<Group>> {
        groups::table.order(groups::name.asc()).load(conn)
    }

    pub fn member_permissions(&self, conn: &mut DbConnection, user_id: i32) -> QueryResult<i32> {
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

    pub fn can_upload(&self, conn: &mut DbConnection, user_id: i32) -> bool {
        self.member_permissions(conn, user_id).map(|p| p & PERM_UPLOAD != 0).unwrap_or(false)
    }

    pub fn can_edit(&self, conn: &mut DbConnection, user_id: i32) -> bool {
        self.member_permissions(conn, user_id).map(|p| p & PERM_EDIT != 0).unwrap_or(false)
    }

    pub fn members_with_perms(&self, conn: &mut DbConnection) -> QueryResult<Vec<(i32, i32)>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tag_drops_a_leading_tag_only() {
        let g = Group {
            id: 1,
            name: "Test".into(),
            tag: "Test".into(),
            slug: "test".into(),
            description: None,
            created_time: NaiveDateTime::default(),
            owner_id: 1,
        };
        assert_eq!(g.strip_tag("[Test] Show - 01"), "Show - 01");
        assert_eq!(g.strip_tag("[test]Show - 01"), "Show - 01");
        assert_eq!(g.strip_tag("Show [Test] - 01"), "Show [Test] - 01");
        assert_eq!(g.strip_tag("[Other] Show"), "[Other] Show");
        assert_eq!(g.strip_tag("[Test]"), "[Test]");
        assert_eq!(g.strip_tag("[Té"), "[Té");
    }
}
