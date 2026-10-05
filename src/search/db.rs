use diesel::prelude::*;
use crate::db::schema::{nyaa_statistics, nyaa_torrents};
use crate::models::Torrent;

// Diesel has no built-in bitwise AND; define the SQL `&` operator for integer columns.
diesel::infix_operator!(BitAnd, " & ", diesel::sql_types::Integer);

trait BitAndExt: Expression<SqlType = diesel::sql_types::Integer> + Sized {
    fn bitand(self, other: i32) -> BitAnd<Self, diesel::dsl::AsExprOf<i32, diesel::sql_types::Integer>> {
        BitAnd::new(self, other.into_sql::<diesel::sql_types::Integer>())
    }
}

impl<T: Expression<SqlType = diesel::sql_types::Integer>> BitAndExt for T {}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub term: Option<String>,
    pub user_id: Option<i32>,
    pub group_id: Option<i32>,
    pub main_category: Option<i32>,
    pub sub_category: Option<i32>,
    pub quality_filter: u8, // 0=all,1=no-remake,2=trusted,3=trusted+complete
    pub sort: SearchSort,
    pub order: SearchOrder,
    pub page: i64,
    pub per_page: i64,
    pub include_deleted: bool,
    pub include_hidden: bool,
    /// Leave out anonymous uploads (set on profile pages for other viewers).
    pub hide_anonymous: bool,
    /// The logged-in visitor. In the general listing they also see their own hidden uploads.
    pub viewer_id: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
pub enum SearchSort {
    Id,
    Name,
    Size,
    Comments,
    Seeders,
    Leechers,
    Downloads,
}

#[derive(Debug, Clone, Copy)]
pub enum SearchOrder {
    Asc,
    Desc,
}

impl SearchQuery {
    /// Defaults for the main listing; only the tests build queries by hand.
    #[cfg(test)]
    pub fn new() -> Self {
        SearchQuery {
            term: None,
            user_id: None,
            group_id: None,
            main_category: None,
            sub_category: None,
            quality_filter: 0,
            sort: SearchSort::Id,
            order: SearchOrder::Desc,
            page: 1,
            per_page: 75,
            include_deleted: false,
            include_hidden: false,
            hide_anonymous: false,
            viewer_id: None,
        }
    }

    pub fn from_params(
        term: Option<String>,
        user_id: Option<i32>,
        group_id: Option<i32>,
        cat: Option<&str>,
        filter: Option<&str>,
        sort: Option<&str>,
        order: Option<&str>,
        page: Option<i64>,
        per_page: i64,
        is_admin: bool,
    ) -> Self {
        let (main_cat, sub_cat) = parse_category(cat);
        let quality_filter = filter.and_then(|f| f.parse().ok()).unwrap_or(0);
        let sort = match sort {
            Some("name") | Some("2") => SearchSort::Name,
            Some("comments") | Some("3") => SearchSort::Comments,
            Some("size") | Some("4") => SearchSort::Size,
            Some("seeders") | Some("5") => SearchSort::Seeders,
            Some("leechers") | Some("6") => SearchSort::Leechers,
            Some("downloads") | Some("7") => SearchSort::Downloads,
            _ => SearchSort::Id,
        };
        let order = match order {
            Some("asc") => SearchOrder::Asc,
            _ => SearchOrder::Desc,
        };
        SearchQuery {
            term: term.filter(|t| !t.is_empty()),
            user_id,
            group_id,
            main_category: main_cat,
            sub_category: sub_cat,
            quality_filter,
            sort,
            order,
            page: page.unwrap_or(1).max(1),
            per_page,
            include_deleted: is_admin,
            include_hidden: is_admin,
            hide_anonymous: false,
            viewer_id: None,
        }
    }
}

fn parse_category(cat: Option<&str>) -> (Option<i32>, Option<i32>) {
    let cat = match cat { Some(c) => c, None => return (None, None) };
    let parts: Vec<&str> = cat.splitn(2, '_').collect();
    if parts.len() != 2 { return (None, None); }
    let main: i32 = parts[0].parse().unwrap_or(0);
    let sub: i32 = parts[1].parse().unwrap_or(0);
    if main == 0 { (None, None) } else { (Some(main), if sub == 0 { None } else { Some(sub) }) }
}

#[derive(Debug)]
pub struct SearchResult {
    pub torrents: Vec<Torrent>,
    pub total: i64,
}

use crate::models::TorrentFlags;

type Listing = diesel::dsl::IntoBoxed<
    'static,
    diesel::dsl::LeftJoin<nyaa_torrents::table, nyaa_statistics::table>,
    diesel::sqlite::Sqlite,
>;

/// Builds the filtered (unsorted, unpaged) query. Used for both the count
/// and the page so the two can't disagree on what is visible. Statistics are
/// joined so the listing can sort by them.
fn filtered(q: &SearchQuery) -> Listing {
    let mut query = nyaa_torrents::table.left_join(nyaa_statistics::table).into_boxed();

    // Term search (LIKE on display_name)
    if let Some(ref term) = q.term {
        query = query.filter(nyaa_torrents::display_name.like(format!("%{}%", term)));
    }

    // User filter
    if let Some(uid) = q.user_id {
        query = query.filter(nyaa_torrents::uploader_id.eq(uid));
    }

    // Group filter
    if let Some(gid) = q.group_id {
        query = query.filter(nyaa_torrents::group_id.eq(gid));
    }

    // Category filter
    if let Some(main) = q.main_category {
        query = query.filter(nyaa_torrents::main_category_id.eq(main));
        if let Some(sub) = q.sub_category {
            query = query.filter(nyaa_torrents::sub_category_id.eq(sub));
        }
    }

    // Quality filter
    let remake_bit = TorrentFlags::REMAKE.bits();
    let trusted_bit = TorrentFlags::TRUSTED.bits();
    let complete_bit = TorrentFlags::COMPLETE.bits();
    match q.quality_filter {
        1 => { query = query.filter(diesel::dsl::not(nyaa_torrents::flags.bitand(remake_bit).ne(0))); }
        2 => { query = query.filter(nyaa_torrents::flags.bitand(trusted_bit).ne(0)); }
        3 => { query = query.filter(nyaa_torrents::flags.bitand(trusted_bit).ne(0))
                           .filter(nyaa_torrents::flags.bitand(complete_bit).ne(0)); }
        _ => {}
    }

    // Hide deleted/banned unless admin
    if !q.include_deleted {
        let deleted_banned = TorrentFlags::DELETED.bits() | TorrentFlags::BANNED.bits();
        query = query.filter(nyaa_torrents::flags.bitand(deleted_banned).eq(0));
    }

    // Hidden torrents are reachable by link only, including on the uploader's profile.
    // In the general listing, logged-in visitors still see their own (upstream behavior).
    if !q.include_hidden {
        let not_hidden = nyaa_torrents::flags.bitand(TorrentFlags::HIDDEN.bits()).eq(0);
        query = match (q.viewer_id, q.user_id) {
            (Some(viewer), None) => query.filter(not_hidden.or(nyaa_torrents::uploader_id.eq(viewer))),
            _ => query.filter(not_hidden),
        };
    }

    // Anonymous torrents must not be tied to their uploader in listings
    if q.hide_anonymous {
        query = query.filter(nyaa_torrents::flags.bitand(TorrentFlags::ANONYMOUS.bits()).eq(0));
    }

    query
}

pub fn search(conn: &mut SqliteConnection, q: &SearchQuery) -> QueryResult<SearchResult> {
    let total: i64 = filtered(q).count().get_result(conn)?;
    let query = filtered(q);

    // Sort, newest first among equal values
    let offset = (q.page - 1) * q.per_page;
    let query = query.select(Torrent::as_select());
    let query = match (q.sort, q.order) {
        (SearchSort::Id, SearchOrder::Desc) => query.order(nyaa_torrents::id.desc()),
        (SearchSort::Id, SearchOrder::Asc) => query.order(nyaa_torrents::id.asc()),
        (SearchSort::Name, SearchOrder::Desc) => query.order(nyaa_torrents::display_name.desc()),
        (SearchSort::Name, SearchOrder::Asc) => query.order(nyaa_torrents::display_name.asc()),
        (SearchSort::Size, SearchOrder::Desc) => query.order(nyaa_torrents::filesize.desc()),
        (SearchSort::Size, SearchOrder::Asc) => query.order(nyaa_torrents::filesize.asc()),
        (SearchSort::Comments, SearchOrder::Desc) => query.order(nyaa_torrents::comment_count.desc()),
        (SearchSort::Comments, SearchOrder::Asc) => query.order(nyaa_torrents::comment_count.asc()),
        (SearchSort::Seeders, SearchOrder::Desc) => query.order(nyaa_statistics::seed_count.desc()),
        (SearchSort::Seeders, SearchOrder::Asc) => query.order(nyaa_statistics::seed_count.asc()),
        (SearchSort::Leechers, SearchOrder::Desc) => query.order(nyaa_statistics::leech_count.desc()),
        (SearchSort::Leechers, SearchOrder::Asc) => query.order(nyaa_statistics::leech_count.asc()),
        (SearchSort::Downloads, SearchOrder::Desc) => query.order(nyaa_statistics::download_count.desc()),
        (SearchSort::Downloads, SearchOrder::Asc) => query.order(nyaa_statistics::download_count.asc()),
    };
    let torrents = query
        .then_order_by(nyaa_torrents::id.desc())
        .limit(q.per_page)
        .offset(offset)
        .load::<Torrent>(conn)?;

    Ok(SearchResult { torrents, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TorrentFlags;
    use diesel_migrations::MigrationHarness;

    fn db_with(torrents: &[(i32, TorrentFlags)]) -> SqliteConnection {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        conn.run_pending_migrations(crate::MIGRATIONS).unwrap();
        diesel::sql_query("INSERT INTO users (id, username, password_hash) VALUES (1, 'u', 'x')")
            .execute(&mut conn).unwrap();
        for (id, flags) in torrents {
            diesel::sql_query(format!(
                "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, \
                 uploader_id, main_category_id, sub_category_id) VALUES ({id}, X'{id:040x}', 't', 't', {}, 1, 1, 1)",
                flags.bits()
            )).execute(&mut conn).unwrap();
        }
        conn
    }

    fn ids(conn: &mut SqliteConnection, q: &SearchQuery) -> (Vec<i32>, i64) {
        let r = search(conn, q).unwrap();
        (r.torrents.iter().map(|t| t.id).collect(), r.total)
    }

    #[test]
    fn profile_hides_hidden_and_anonymous_from_other_viewers() {
        let mut conn = db_with(&[
            (1, TorrentFlags::empty()),
            (2, TorrentFlags::HIDDEN),
            (3, TorrentFlags::ANONYMOUS),
            (4, TorrentFlags::DELETED),
        ]);
        let mut q = SearchQuery::new();
        q.user_id = Some(1);

        // Another viewer on the profile
        q.hide_anonymous = true;
        assert_eq!(ids(&mut conn, &q), (vec![1], 1));

        // The owner (or a moderator) on the profile
        q.hide_anonymous = false;
        q.include_hidden = true;
        assert_eq!(ids(&mut conn, &q), (vec![3, 2, 1], 3));
    }

    #[test]
    fn main_listing_shows_anonymous_but_not_hidden() {
        let mut conn = db_with(&[
            (1, TorrentFlags::empty()),
            (2, TorrentFlags::HIDDEN),
            (3, TorrentFlags::ANONYMOUS),
        ]);
        assert_eq!(ids(&mut conn, &SearchQuery::new()), (vec![3, 1], 2));
    }

    #[test]
    fn logged_in_viewer_sees_own_hidden_in_general_listing() {
        let mut conn = db_with(&[(1, TorrentFlags::empty()), (2, TorrentFlags::HIDDEN)]);
        diesel::sql_query("INSERT INTO users (id, username, password_hash) VALUES (2, 'v', 'x')")
            .execute(&mut conn).unwrap();
        let mut q = SearchQuery::new();

        q.viewer_id = Some(1); // the uploader
        assert_eq!(ids(&mut conn, &q), (vec![2, 1], 2));
        q.viewer_id = Some(2); // someone else
        assert_eq!(ids(&mut conn, &q), (vec![1], 1));
    }

    #[test]
    fn sorts_by_statistics() {
        let mut conn = db_with(&[(1, TorrentFlags::empty()), (2, TorrentFlags::empty()), (3, TorrentFlags::empty())]);
        diesel::sql_query("INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) \
                           VALUES (1, 5, 0, 9), (2, 50, 3, 1)")
            .execute(&mut conn).unwrap();
        let mut q = SearchQuery::new();

        q.sort = SearchSort::Seeders;
        assert_eq!(ids(&mut conn, &q).0, vec![2, 1, 3]); // no stats row sorts last
        q.sort = SearchSort::Downloads;
        assert_eq!(ids(&mut conn, &q).0, vec![1, 2, 3]);
        q.order = SearchOrder::Asc;
        q.sort = SearchSort::Leechers;
        assert_eq!(ids(&mut conn, &q).0, vec![3, 1, 2]);
    }
}
