use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::{group_reports, nyaa_reports};
use crate::db::DbConnection;

/// Upstream ReportStatus, stored in `status` of both report tables.
pub const REPORT_IN_REVIEW: i32 = 0;
/// The torrent was hidden or deleted because of the report.
pub const REPORT_VALID: i32 = 1;
/// Closed without action.
pub const REPORT_INVALID: i32 = 2;

/// Upstream ReportForm: the reason is 3 to 255 characters.
pub const REPORT_REASON_MIN: usize = 3;
pub const REPORT_REASON_MAX: usize = 255;

/// Upstream's per_page for the admin queue.
pub const REPORTS_PER_PAGE: i64 = 20;

/// The trimmed reason, or the message upstream's form shows when it is missing or out of range.
pub fn validate_reason(reason: &str) -> Result<String, &'static str> {
    let reason = crate::utils::sanitize_string(reason);
    let reason = reason.trim().to_string();
    match reason.chars().count() {
        0 => Err("Please give a report reason!"),
        n if !(REPORT_REASON_MIN..=REPORT_REASON_MAX).contains(&n) => {
            Err("Report reason must be at least 3 characters long and 255 at most.")
        }
        _ => Ok(reason),
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = nyaa_reports)]
pub struct Report {
    pub id: i32,
    pub created_time: NaiveDateTime,
    pub reason: String,
    pub status: i32,
    pub torrent_id: i32,
    pub user_id: Option<i32>,
}

impl Report {
    pub fn create(conn: &mut DbConnection, torrent_id: i32, user_id: i32, reason: &str) -> QueryResult<usize> {
        diesel::insert_into(nyaa_reports::table)
            .values((
                nyaa_reports::torrent_id.eq(torrent_id),
                nyaa_reports::user_id.eq(Some(user_id)),
                nyaa_reports::reason.eq(reason),
                nyaa_reports::status.eq(REPORT_IN_REVIEW),
                nyaa_reports::created_time.eq(chrono::Utc::now().naive_utc()),
            ))
            .execute(conn)
    }

    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<Report>> {
        nyaa_reports::table.find(id).first(conn).optional()
    }

    /// One page of reports still in review, oldest first, and how many there are in all.
    pub fn not_reviewed(conn: &mut DbConnection, page: i64) -> QueryResult<(Vec<Report>, i64)> {
        let total = nyaa_reports::table.filter(nyaa_reports::status.eq(REPORT_IN_REVIEW)).count().get_result(conn)?;
        let rows = nyaa_reports::table
            .filter(nyaa_reports::status.eq(REPORT_IN_REVIEW))
            .order(nyaa_reports::id.asc())
            .limit(REPORTS_PER_PAGE)
            .offset((page.max(1) - 1) * REPORTS_PER_PAGE)
            .load(conn)?;
        Ok((rows, total))
    }

    /// Gives every report on the torrent still in review the outcome `status`. Upstream
    /// deletes the other open reports on the torrent instead; this keeps them for the record.
    pub fn review_all(conn: &mut DbConnection, torrent_id: i32, status: i32) -> QueryResult<usize> {
        diesel::update(
            nyaa_reports::table
                .filter(nyaa_reports::torrent_id.eq(torrent_id))
                .filter(nyaa_reports::status.eq(REPORT_IN_REVIEW)),
        )
        .set(nyaa_reports::status.eq(status))
        .execute(conn)
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = group_reports)]
pub struct GroupReport {
    pub id: i32,
    pub created_time: NaiveDateTime,
    pub reason: String,
    pub status: i32,
    pub group_id: i32,
    pub user_id: Option<i32>,
}

impl GroupReport {
    pub fn create(conn: &mut DbConnection, group_id: i32, user_id: i32, reason: &str) -> QueryResult<usize> {
        diesel::insert_into(group_reports::table)
            .values((
                group_reports::group_id.eq(group_id),
                group_reports::user_id.eq(Some(user_id)),
                group_reports::reason.eq(reason),
                group_reports::status.eq(REPORT_IN_REVIEW),
                group_reports::created_time.eq(chrono::Utc::now().naive_utc()),
            ))
            .execute(conn)
    }

    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<GroupReport>> {
        group_reports::table.find(id).first(conn).optional()
    }

    pub fn not_reviewed(conn: &mut DbConnection, page: i64) -> QueryResult<(Vec<GroupReport>, i64)> {
        let total = group_reports::table.filter(group_reports::status.eq(REPORT_IN_REVIEW)).count().get_result(conn)?;
        let rows = group_reports::table
            .filter(group_reports::status.eq(REPORT_IN_REVIEW))
            .order(group_reports::id.asc())
            .limit(REPORTS_PER_PAGE)
            .offset((page.max(1) - 1) * REPORTS_PER_PAGE)
            .load(conn)?;
        Ok((rows, total))
    }

    pub fn review_all(conn: &mut DbConnection, group_id: i32, status: i32) -> QueryResult<usize> {
        diesel::update(
            group_reports::table
                .filter(group_reports::group_id.eq(group_id))
                .filter(group_reports::status.eq(REPORT_IN_REVIEW)),
        )
        .set(group_reports::status.eq(status))
        .execute(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_must_be_3_to_255_characters() {
        assert_eq!(validate_reason("  "), Err("Please give a report reason!"));
        assert!(validate_reason("ab").is_err());
        assert_eq!(validate_reason("  fake \u{0}"), Ok("fake".into()));
        assert!(validate_reason(&"é".repeat(255)).is_ok());
        assert!(validate_reason(&"é".repeat(256)).is_err());
    }
}
