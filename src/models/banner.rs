use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::Serialize;

use crate::db::DbConnection;
use crate::db::schema::{site_banners, users};

/// Text shown above the torrent list on the main page while `active` (Admin > Banners).
#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = site_banners)]
pub struct Banner {
    pub id: i32,
    pub content: String,
    pub active: bool,
    pub created_time: NaiveDateTime,
    pub created_by: i32,
}

/// A banner with its creator's username, for the admin list.
#[derive(Debug, Serialize)]
pub struct BannerRow {
    #[serde(flatten)]
    pub banner: Banner,
    pub creator: String,
}

impl Banner {
    /// The banners shown on the main page, newest first.
    pub fn active(conn: &mut DbConnection) -> QueryResult<Vec<Banner>> {
        site_banners::table
            .filter(site_banners::active.eq(true))
            .order(site_banners::id.desc())
            .load(conn)
    }

    pub fn all_with_creator(conn: &mut DbConnection) -> QueryResult<Vec<BannerRow>> {
        let rows: Vec<(Banner, String)> = site_banners::table
            .inner_join(users::table)
            .select((Banner::as_select(), users::username))
            .order(site_banners::id.desc())
            .load(conn)?;
        Ok(rows.into_iter().map(|(banner, creator)| BannerRow { banner, creator }).collect())
    }

    /// Saves a new banner, active right away.
    pub fn create(conn: &mut DbConnection, content: &str, created_by: i32) -> QueryResult<()> {
        diesel::insert_into(site_banners::table)
            .values((
                site_banners::content.eq(content),
                site_banners::active.eq(true),
                site_banners::created_time.eq(chrono::Utc::now().naive_utc()),
                site_banners::created_by.eq(created_by),
            ))
            .execute(conn)
            .map(|_| ())
    }

    /// Flips the banner on or off; returns its new state, or None if it does not exist.
    pub fn toggle(conn: &mut DbConnection, id: i32) -> QueryResult<Option<bool>> {
        let Some(active) = site_banners::table.find(id).select(site_banners::active)
            .first::<bool>(conn).optional()? else { return Ok(None) };
        diesel::update(site_banners::table.find(id))
            .set(site_banners::active.eq(!active))
            .execute(conn)?;
        Ok(Some(!active))
    }

    /// Returns whether a row was deleted.
    pub fn delete(conn: &mut DbConnection, id: i32) -> QueryResult<bool> {
        Ok(diesel::delete(site_banners::table.find(id)).execute(conn)? > 0)
    }
}
