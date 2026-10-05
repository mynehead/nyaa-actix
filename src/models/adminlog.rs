use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;

use crate::db::DbConnection;
use crate::db::schema::{adminlog, users};

/// A row of `adminlog`: one moderator action, shown on /admin/log. `log` is Markdown
/// rendered inline on the page, using upstream's wording, for example
/// `Torrent [#5](/view/5) has been deleted`.
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = adminlog)]
pub struct AdminLog {
    pub id: i32,
    pub created_time: NaiveDateTime,
    pub log: String,
    pub admin_id: i32,
}

/// A log row with its moderator's name, as the log page lists it.
#[derive(Debug, Clone, Serialize)]
pub struct AdminLogEntry {
    #[serde(flatten)]
    pub entry: AdminLog,
    pub admin_name: String,
}

/// Upstream caps the column at 1024 characters.
pub const MAX_LOG_LEN: usize = 1024;

impl AdminLog {
    /// Records a moderator action. Call it inside the transaction that makes the change,
    /// so the log and the change land together.
    pub fn add(conn: &mut DbConnection, admin_id: i32, log: &str) -> QueryResult<()> {
        let log: String = log.chars().take(MAX_LOG_LEN).collect();
        diesel::insert_into(adminlog::table)
            .values((
                adminlog::created_time.eq(chrono::Utc::now().naive_utc()),
                adminlog::log.eq(log),
                adminlog::admin_id.eq(admin_id),
            ))
            .execute(conn)
            .map(|_| ())
    }

    /// One page of the log, newest first, and the total number of entries.
    pub fn page(conn: &mut DbConnection, page: i64, per_page: i64) -> QueryResult<(Vec<AdminLogEntry>, i64)> {
        let total = adminlog::table.count().get_result(conn)?;
        let rows = adminlog::table
            .inner_join(users::table)
            .order((adminlog::created_time.desc(), adminlog::id.desc()))
            .limit(per_page)
            .offset((page.max(1) - 1) * per_page)
            .select((AdminLog::as_select(), users::username))
            .load::<(AdminLog, String)>(conn)?;
        let entries = rows.into_iter()
            .map(|(entry, admin_name)| AdminLogEntry { entry, admin_name })
            .collect();
        Ok((entries, total))
    }
}

/// Upstream's log text for a torrent link: `[#5](/view/5)`.
pub fn torrent_link(torrent_id: i32) -> String {
    format!("[#{0}](/view/{0})", torrent_id)
}

/// Upstream's log text for a user link: `[name](/user/name)`.
pub fn user_link(username: &str) -> String {
    format!("[{0}](/user/{0})", username)
}

/// Replaces every `IP(...)` in a log line with `IP(hidden)`, as upstream does for
/// moderators who are not superadmins.
pub fn hide_ips(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut rest = log;
    while let Some(start) = rest.find("IP(") {
        let after = &rest[start + 3..];
        match after.find(')') {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push_str("IP(hidden)");
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_ips_like_upstream() {
        assert_eq!(hide_ips("User [a](/user/a) IP(1.2.3.4) has been banned."),
                   "User [a](/user/a) IP(hidden) has been banned.");
        assert_eq!(hide_ips("IP(::1) IP(10.0.0.1)"), "IP(hidden) IP(hidden)");
        assert_eq!(hide_ips("no ip here IP(unclosed"), "no ip here IP(unclosed");
    }

    #[test]
    fn links_use_upstream_markdown() {
        assert_eq!(torrent_link(5), "[#5](/view/5)");
        assert_eq!(user_link("bob"), "[bob](/user/bob)");
    }
}
