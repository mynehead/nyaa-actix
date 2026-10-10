//! Applications for trusted status and moderator reviews of them (upstream
//! `TrustedApplication` / `TrustedReview`).

use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;

use crate::config::TrustedConfig;
use crate::db::schema::{nyaa_statistics, nyaa_torrents, trusted_applications, trusted_reviews, users};
use crate::db::DbConnection;
use crate::models::{user_link, AdminLog, TorrentFlags, User, UserLevel};

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedApplicationStatus {
    New = 0,
    Reviewed = 1,
    Accepted = 2,
    Rejected = 3,
}

impl TrustedApplicationStatus {
    /// Accepted and rejected applications are closed.
    pub const FIRST_CLOSED: i32 = TrustedApplicationStatus::Accepted as i32;

    pub fn name(status: i32) -> &'static str {
        match status {
            0 => "New",
            1 => "Reviewed",
            2 => "Accepted",
            3 => "Rejected",
            _ => "Unknown",
        }
    }
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedRecommendation {
    Accept = 0,
    Reject = 1,
    Abstain = 2,
}

impl TrustedRecommendation {
    /// The review form's select values.
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "accept" => Some(Self::Accept),
            "reject" => Some(Self::Reject),
            "abstain" => Some(Self::Abstain),
            _ => None,
        }
    }

    pub fn name(v: i32) -> &'static str {
        match v {
            0 => "accept",
            1 => "reject",
            _ => "abstain",
        }
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = trusted_applications)]
pub struct TrustedApplication {
    pub id: i32,
    pub submitter_id: i32,
    pub created_time: NaiveDateTime,
    pub closed_time: Option<NaiveDateTime>,
    pub why_want: String,
    pub why_give: String,
    pub status: i32,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = trusted_applications)]
pub struct NewTrustedApplication {
    pub submitter_id: i32,
    pub created_time: NaiveDateTime,
    pub why_want: String,
    pub why_give: String,
    pub status: i32,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = trusted_reviews)]
pub struct TrustedReview {
    pub id: i32,
    pub reviewer_id: i32,
    pub app_id: i32,
    pub created_time: NaiveDateTime,
    pub comment: String,
    pub recommendation: i32,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = trusted_reviews)]
pub struct NewTrustedReview {
    pub reviewer_id: i32,
    pub app_id: i32,
    pub created_time: NaiveDateTime,
    pub comment: String,
    pub recommendation: i32,
}

/// The tabs of the admin list: open (new and reviewed), new, reviewed and closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedListFilter {
    Open,
    New,
    Reviewed,
    Closed,
}

impl TrustedListFilter {
    pub fn parse(v: Option<&str>) -> Option<Self> {
        match v {
            None => Some(Self::Open),
            Some("new") => Some(Self::New),
            Some("reviewed") => Some(Self::Reviewed),
            Some("closed") => Some(Self::Closed),
            Some(_) => None,
        }
    }
}

impl TrustedApplication {
    pub fn is_closed(&self) -> bool {
        self.status >= TrustedApplicationStatus::FIRST_CLOSED
    }

    pub fn by_id(conn: &mut DbConnection, id: i32) -> QueryResult<Option<TrustedApplication>> {
        trusted_applications::table.find(id).first(conn).optional()
    }

    /// One page of the admin list, newest first, with each submitter; and the total count.
    pub fn list(
        conn: &mut DbConnection,
        filter: TrustedListFilter,
        page: i64,
        per_page: i64,
    ) -> QueryResult<(Vec<(TrustedApplication, User)>, i64)> {
        let filtered = || {
            let q = trusted_applications::table.into_boxed();
            let closed = TrustedApplicationStatus::FIRST_CLOSED;
            match filter {
                TrustedListFilter::Open => q.filter(trusted_applications::status.lt(closed)),
                TrustedListFilter::New => {
                    q.filter(trusted_applications::status.eq(TrustedApplicationStatus::New as i32))
                }
                TrustedListFilter::Reviewed => {
                    q.filter(trusted_applications::status.eq(TrustedApplicationStatus::Reviewed as i32))
                }
                TrustedListFilter::Closed => q.filter(trusted_applications::status.ge(closed)),
            }
        };
        let total: i64 = filtered().count().get_result(conn)?;
        let apps: Vec<TrustedApplication> = filtered()
            .order((trusted_applications::created_time.desc(), trusted_applications::id.desc()))
            .limit(per_page)
            .offset((page.max(1) - 1) * per_page)
            .load(conn)?;
        let rows = apps
            .into_iter()
            .map(|app| {
                let user = users::table.find(app.submitter_id).first::<User>(conn)?;
                Ok((app, user))
            })
            .collect::<QueryResult<Vec<_>>>()?;
        Ok((rows, total))
    }

    /// Reviews of this application, oldest first, each with its reviewer.
    pub fn reviews(&self, conn: &mut DbConnection) -> QueryResult<Vec<(TrustedReview, User)>> {
        trusted_reviews::table
            .inner_join(users::table.on(users::id.eq(trusted_reviews::reviewer_id)))
            .filter(trusted_reviews::app_id.eq(self.id))
            .order(trusted_reviews::id.asc())
            .select((TrustedReview::as_select(), User::as_select()))
            .load(conn)
    }

    pub fn submit(conn: &mut DbConnection, submitter_id: i32, why_give: &str, why_want: &str) -> QueryResult<usize> {
        diesel::insert_into(trusted_applications::table)
            .values(&NewTrustedApplication {
                submitter_id,
                created_time: chrono::Utc::now().naive_utc(),
                why_want: why_want.to_string(),
                why_give: why_give.to_string(),
                status: TrustedApplicationStatus::New as i32,
            })
            .execute(conn)
    }

    /// Adds a review; a new application becomes reviewed.
    pub fn add_review(
        &self,
        conn: &mut DbConnection,
        reviewer_id: i32,
        comment: &str,
        recommendation: TrustedRecommendation,
    ) -> QueryResult<()> {
        conn.transaction(|conn| {
            diesel::insert_into(trusted_reviews::table)
                .values(&NewTrustedReview {
                    reviewer_id,
                    app_id: self.id,
                    created_time: chrono::Utc::now().naive_utc(),
                    comment: comment.to_string(),
                    recommendation: recommendation as i32,
                })
                .execute(conn)?;
            let log = format!(
                "Trusted application #{} of {}: reviewed, recommends {}",
                self.id,
                self.submitter_link(conn)?,
                TrustedRecommendation::name(recommendation as i32)
            );
            AdminLog::add(conn, reviewer_id, &log)?;
            diesel::update(trusted_applications::table.find(self.id))
                .filter(trusted_applications::status.eq(TrustedApplicationStatus::New as i32))
                .set(trusted_applications::status.eq(TrustedApplicationStatus::Reviewed as i32))
                .execute(conn)?;
            Ok(())
        })
    }

    fn submitter_link(&self, conn: &mut DbConnection) -> QueryResult<String> {
        let name: String = users::table.find(self.submitter_id).select(users::username).first(conn)?;
        Ok(user_link(&name))
    }

    /// Closes the application; accepting makes the submitter trusted. Returns false when
    /// it was already closed.
    pub fn decide(&self, conn: &mut DbConnection, admin_id: i32, accept: bool) -> QueryResult<bool> {
        let status = if accept { TrustedApplicationStatus::Accepted } else { TrustedApplicationStatus::Rejected };
        conn.transaction(|conn| {
            let closed = diesel::update(trusted_applications::table.find(self.id))
                .filter(trusted_applications::status.lt(TrustedApplicationStatus::FIRST_CLOSED))
                .set((
                    trusted_applications::status.eq(status as i32),
                    trusted_applications::closed_time.eq(chrono::Utc::now().naive_utc()),
                ))
                .execute(conn)?;
            if closed == 0 {
                return Ok(false);
            }
            let log = format!(
                "Trusted application #{} of {}: {}",
                self.id,
                self.submitter_link(conn)?,
                if accept { "accepted" } else { "rejected" }
            );
            AdminLog::add(conn, admin_id, &log)?;
            if accept {
                // Upstream sets the level outright; never demote someone promoted since applying
                diesel::update(users::table.find(self.submitter_id))
                    .filter(users::level.lt(UserLevel::Trusted as i32))
                    .set(users::level.eq(UserLevel::Trusted as i32))
                    .execute(conn)?;
            }
            Ok(true)
        })
    }
}

/// Why `user` may not apply for trusted status right now; empty when they may.
pub fn trusted_deny_reasons(conn: &mut DbConnection, user: &User, cfg: &TrustedConfig) -> QueryResult<Vec<String>> {
    let mut reasons = Vec::new();
    if user.level() >= UserLevel::Trusted {
        reasons.push("You are already trusted.".to_string());
    }
    if !satisfies_trusted_reqs(conn, user.id, cfg)? {
        reasons.push("You do not satisfy the minimum requirements.".to_string());
    }
    let open: i64 = trusted_applications::table
        .filter(trusted_applications::submitter_id.eq(user.id))
        .filter(trusted_applications::status.lt(TrustedApplicationStatus::FIRST_CLOSED))
        .count()
        .get_result(conn)?;
    if open > 0 {
        reasons.push("You already have an open application.".to_string());
    }
    let last_rejected: Option<Option<NaiveDateTime>> = trusted_applications::table
        .filter(trusted_applications::submitter_id.eq(user.id))
        .filter(trusted_applications::status.eq(TrustedApplicationStatus::Rejected as i32))
        .order(trusted_applications::closed_time.desc())
        .select(trusted_applications::closed_time)
        .first(conn)
        .optional()?;
    if let Some(Some(closed)) = last_rejected {
        if (chrono::Utc::now().naive_utc() - closed).num_days() < cfg.reapply_cooldown_days {
            reasons
                .push(format!("Your last application was rejected less than {} days ago.", cfg.reapply_cooldown_days));
        }
    }
    Ok(reasons)
}

/// Upstream `User.satisfies_trusted_reqs`: enough uploads that are not remakes, and
/// enough downloads of them.
fn satisfies_trusted_reqs(conn: &mut DbConnection, user_id: i32, cfg: &TrustedConfig) -> QueryResult<bool> {
    use diesel::dsl::{count_star, sql, sum};
    let not_remake =
        || sql::<diesel::sql_types::Bool>(&format!("(nyaa_torrents.flags & {}) = 0", TorrentFlags::REMAKE.bits()));
    let uploads: i64 = nyaa_torrents::table
        .filter(nyaa_torrents::uploader_id.eq(user_id))
        .filter(not_remake())
        .select(count_star())
        .get_result(conn)?;
    let downloads: Option<i64> = nyaa_statistics::table
        .inner_join(nyaa_torrents::table)
        .filter(nyaa_torrents::uploader_id.eq(user_id))
        .filter(not_remake())
        .select(sum(nyaa_statistics::download_count))
        .get_result(conn)?;
    Ok(uploads >= cfg.min_uploads && downloads.unwrap_or(0) >= cfg.min_downloads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> DbConnection {
        let mut conn = crate::db::connect(":memory:").unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
                           (1, 'uploader', 'x', 1, 0), (2, 'mod', 'x', 1, 2)",
        )
        .execute(&mut conn)
        .unwrap();
        conn
    }

    fn add_torrent(conn: &mut DbConnection, id: i32, flags: i32, downloads: i32) {
        diesel::sql_query(format!(
            "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, information, description, \
             flags, uploader_id, main_category_id, sub_category_id) \
             VALUES ({id}, X'{}', 't', 't.torrent', '', '', {flags}, 1, 1, 2)",
            format!("{id:02x}").repeat(20)
        ))
        .execute(conn)
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) \
                                   VALUES ({id}, 0, 0, {downloads})"
        ))
        .execute(conn)
        .unwrap();
    }

    fn user(conn: &mut DbConnection, id: i32) -> User {
        User::by_id(conn, id).unwrap().unwrap()
    }

    #[test]
    fn requirements_skip_remakes() {
        let mut conn = conn();
        let cfg = TrustedConfig { applications: true, min_uploads: 2, min_downloads: 100, reapply_cooldown_days: 90 };
        add_torrent(&mut conn, 1, 0, 60);
        add_torrent(&mut conn, 2, TorrentFlags::REMAKE.bits(), 1000);
        assert!(!satisfies_trusted_reqs(&mut conn, 1, &cfg).unwrap());
        add_torrent(&mut conn, 3, TorrentFlags::TRUSTED.bits(), 40);
        assert!(satisfies_trusted_reqs(&mut conn, 1, &cfg).unwrap());
        assert!(!satisfies_trusted_reqs(&mut conn, 2, &cfg).unwrap());
    }

    #[test]
    fn review_then_accept_makes_user_trusted() {
        let mut conn = conn();
        let cfg = TrustedConfig { applications: true, min_uploads: 0, min_downloads: 0, reapply_cooldown_days: 90 };
        let u = user(&mut conn, 1);
        assert!(trusted_deny_reasons(&mut conn, &u, &cfg).unwrap().is_empty());
        TrustedApplication::submit(&mut conn, 1, "give", "want").unwrap();
        assert_eq!(trusted_deny_reasons(&mut conn, &u, &cfg).unwrap(), vec!["You already have an open application."]);

        let app = TrustedApplication::by_id(&mut conn, 1).unwrap().unwrap();
        let (rows, total) = TrustedApplication::list(&mut conn, TrustedListFilter::New, 1, 20).unwrap();
        assert_eq!((rows.len(), total, rows[0].1.username.as_str()), (1, 1, "uploader"));
        app.add_review(&mut conn, 2, "looks fine", TrustedRecommendation::Accept).unwrap();
        let app = TrustedApplication::by_id(&mut conn, 1).unwrap().unwrap();
        assert_eq!(app.status, TrustedApplicationStatus::Reviewed as i32);
        assert_eq!(TrustedApplication::list(&mut conn, TrustedListFilter::New, 1, 20).unwrap().1, 0);
        assert_eq!(TrustedApplication::list(&mut conn, TrustedListFilter::Open, 1, 20).unwrap().1, 1);
        assert_eq!(app.reviews(&mut conn).unwrap()[0].1.username, "mod");

        assert!(app.decide(&mut conn, 2, true).unwrap());
        assert!(!app.decide(&mut conn, 2, false).unwrap(), "a closed application stays closed");
        assert_eq!(TrustedApplication::list(&mut conn, TrustedListFilter::Closed, 1, 20).unwrap().1, 1);
        let u = user(&mut conn, 1);
        assert_eq!(u.level(), UserLevel::Trusted);
        let (log, _) = AdminLog::page(&mut conn, 1, 10).unwrap();
        let log: Vec<&str> = log.iter().map(|e| e.entry.log.as_str()).collect();
        assert_eq!(
            log,
            vec![
                "Trusted application #1 of [uploader](/user/uploader): accepted",
                "Trusted application #1 of [uploader](/user/uploader): reviewed, recommends accept"
            ]
        );
        assert_eq!(trusted_deny_reasons(&mut conn, &u, &cfg).unwrap(), vec!["You are already trusted."]);
    }

    #[test]
    fn rejection_starts_cooldown() {
        let mut conn = conn();
        let cfg = TrustedConfig { applications: true, min_uploads: 0, min_downloads: 0, reapply_cooldown_days: 90 };
        TrustedApplication::submit(&mut conn, 1, "give", "want").unwrap();
        let app = TrustedApplication::by_id(&mut conn, 1).unwrap().unwrap();
        assert!(app.decide(&mut conn, 2, false).unwrap());
        let u = user(&mut conn, 1);
        assert_eq!(u.level(), UserLevel::Regular);
        assert_eq!(
            trusted_deny_reasons(&mut conn, &u, &cfg).unwrap(),
            vec!["Your last application was rejected less than 90 days ago."]
        );
        diesel::sql_query("UPDATE trusted_applications SET closed_time = '2020-01-01 00:00:00'")
            .execute(&mut conn)
            .unwrap();
        assert!(trusted_deny_reasons(&mut conn, &u, &cfg).unwrap().is_empty());
    }
}
