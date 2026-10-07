pub mod db;
pub mod index;
pub mod meili;
pub mod syntax;

use diesel::prelude::*;

use crate::db::schema::nyaa_torrents;
use crate::db::DbConnection;
use crate::models::Torrent;
use db::{SearchQuery, SearchResult, SearchSort};
use meili::Meili;

/// Whether a listing goes to Meilisearch when it is configured: text searches and the
/// stats sorts, which SQLite can only answer by scanning every torrent. Plain listings
/// stay on SQLite, where the indexes already make them fast, as upstream keeps
/// term-less listings on SQL.
fn wants_index(q: &SearchQuery) -> bool {
    q.term.is_some() || matches!(q.sort, SearchSort::Seeders | SearchSort::Leechers | SearchSort::Downloads)
}

/// One page of a listing. Uses Meilisearch where it helps and falls back to SQLite when it
/// is not configured or fails, so search never breaks because the index is down.
pub fn search(conn: &mut DbConnection, meili: Option<&Meili>, q: &SearchQuery) -> QueryResult<SearchResult> {
    let r = q.resolve(conn)?;
    if let Some(meili) = meili.filter(|m| m.is_ready() && wants_index(q) && !r.matches_nothing) {
        match meili.search(q, &r) {
            Ok((ids, total)) => return Ok(SearchResult { torrents: load_in_order(conn, &ids)?, total }),
            Err(e) => log::warn!("Meilisearch search failed, using SQLite: {e}"),
        }
    }
    db::search_resolved(conn, q, &r)
}

/// Loads torrents by id in the given order, skipping any gone from SQLite since they were
/// indexed.
fn load_in_order(conn: &mut DbConnection, ids: &[i32]) -> QueryResult<Vec<Torrent>> {
    let mut by_id: std::collections::HashMap<i32, Torrent> = nyaa_torrents::table
        .filter(nyaa_torrents::id.eq_any(ids))
        .load::<Torrent>(conn)?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TorrentFlags;
    use std::time::Duration;

    /// Torrents 1..=6: names, flags, categories, uploaders and seeders that the tests below
    /// filter and sort on.
    fn db() -> DbConnection {
        let mut conn = crate::db::connect(":memory:").unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query("INSERT INTO users (id, username, password_hash) VALUES (1, 'a', 'x'), (2, 'b', 'x')")
            .execute(&mut conn)
            .unwrap();
        diesel::sql_query("INSERT INTO groups (id, name, tag, slug, owner_id) VALUES (1, 'Grp', 'Grp', 'grp', 1)")
            .execute(&mut conn)
            .unwrap();
        let rows = [
            (1, "[Grp] Dragon Show - 01 [1080p]", TorrentFlags::empty(), 1, 1, 1, 10),
            (2, "[Grp] Dragon Show - 02 [720p]", TorrentFlags::TRUSTED, 1, 2, 1, 50),
            (3, "[Other] Sword Tale - 01 [1080p]", TorrentFlags::REMAKE, 2, 1, 2, 30),
            (4, "[Grp] Dragon Show - 03 [1080p]", TorrentFlags::HIDDEN, 1, 1, 1, 99),
            (5, "[Grp] Dragon Show - 04 [1080p]", TorrentFlags::DELETED, 1, 1, 1, 0),
            (
                6,
                "[Other] Dragon Movie [1080p]",
                TorrentFlags::ANONYMOUS | TorrentFlags::TRUSTED | TorrentFlags::COMPLETE,
                1,
                2,
                2,
                20,
            ),
        ];
        for (id, name, flags, main, sub, uploader, seeders) in rows {
            diesel::sql_query(format!(
                "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, uploader_id, \
                 main_category_id, sub_category_id, filesize) VALUES ({id}, X'{id:040x}', '{name}', 't', {}, {uploader}, {main}, {sub}, {})",
                flags.bits(), id * 100
            )).execute(&mut conn).unwrap();
            if name.starts_with("[Grp]") {
                diesel::sql_query(format!("UPDATE nyaa_torrents SET group_id = 1 WHERE id = {id}"))
                    .execute(&mut conn)
                    .unwrap();
            }
            diesel::sql_query(format!(
                "INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) VALUES ({id}, {seeders}, 0, {id})"
            )).execute(&mut conn).unwrap();
        }
        conn
    }

    fn query(
        term: Option<&str>,
        cat: Option<&str>,
        filter: Option<&str>,
        sort: Option<&str>,
        order: Option<&str>,
    ) -> SearchQuery {
        SearchQuery::from_params(term.map(String::from), None, None, cat, filter, sort, order, None, 75, false)
    }

    fn ids(conn: &mut DbConnection, meili: Option<&Meili>, q: &SearchQuery) -> (Vec<i32>, i64) {
        let r = search(conn, meili, q).unwrap();
        (r.torrents.iter().map(|t| t.id).collect(), r.total)
    }

    #[test]
    fn plain_listings_stay_on_sqlite() {
        assert!(!wants_index(&query(None, Some("1_2"), Some("2"), Some("size"), None)));
        assert!(wants_index(&query(Some("dragon"), None, None, None, None)));
        assert!(wants_index(&query(None, None, None, Some("seeders"), None)));
        assert!(wants_index(&query(None, None, None, Some("downloads"), Some("asc"))));
    }

    /// Operator searches, as (term, moderator, viewer) and the ids they find. The
    /// Meilisearch round trip below checks the index agrees on every one.
    fn operator_cases() -> Vec<(SearchQuery, Vec<i32>)> {
        let case = |term: &str, moderator: bool, viewer: Option<i32>, want: &[i32]| {
            let mut q = query(Some(term), None, None, None, None);
            q.moderator = moderator;
            q.include_deleted = moderator;
            q.include_hidden = moderator;
            q.viewer_id = viewer;
            (q, want.to_vec())
        };
        vec![
            case("dragon -720p", false, None, &[6, 1]),
            case(r#""dragon show""#, false, None, &[2, 1]),
            case(r#"dragon -"dragon show""#, false, None, &[6]),
            case("!dragon", false, None, &[3]),
            case("user:a", false, None, &[2, 1]),
            case("user:A dragon -720p", false, None, &[1]),
            // Anonymous uploads stay unattributed, both ways, except to moderators and the
            // uploader
            case("user:b", false, None, &[3]),
            case("-user:b dragon", false, None, &[6, 2, 1]),
            case("user:b", false, Some(2), &[6, 3]),
            case("user:b", true, None, &[6, 3]),
            case("-user:b", true, None, &[5, 4, 2, 1]),
            case("group:grp", false, None, &[2, 1]),
            case("-group:GRP", false, None, &[6, 3]),
            case("g:grp u:b", false, None, &[]),
            // Unknown names: nothing to include, nothing to leave out
            case("user:nobody dragon", false, None, &[]),
            case("group:nothing", false, None, &[]),
            case("-user:nobody sword", false, None, &[3]),
        ]
    }

    #[test]
    fn operators_filter_in_sqlite() {
        let mut conn = db();
        for (q, want) in operator_cases() {
            let total = want.len() as i64;
            assert_eq!(ids(&mut conn, None, &q), (want, total), "{q:?}");
        }
    }

    #[test]
    fn falls_back_to_sqlite_when_meilisearch_is_down() {
        let mut conn = db();
        // Nothing listens on port 1
        let meili = Meili::new("http://127.0.0.1:1", None, "torrents", 1000);
        meili.set_ready(true);
        let q = query(Some("Dragon Show"), None, None, None, None);
        assert_eq!(ids(&mut conn, Some(&meili), &q), (vec![2, 1], 2));

        // An update that can't reach the index keeps searches off it until a rebuild
        index::torrent_changed(&mut conn, Some(&meili), 1);
        assert!(!meili.is_ready());
        assert!(meili.take_stale());
    }

    #[test]
    fn index_is_not_used_until_checked() {
        let mut conn = db();
        let meili = Meili::new("http://127.0.0.1:1", None, "torrents", 1000);
        assert!(!meili.is_ready());
        // Not even tried: no warning, straight to SQLite
        let q = query(Some("Dragon Show"), None, None, None, None);
        assert_eq!(ids(&mut conn, Some(&meili), &q), (vec![2, 1], 2));
        assert!(index::check(&mut conn, &meili).is_err());
        assert!(!meili.is_ready());
    }

    /// Runs against a real Meilisearch when MEILI_TEST_URL is set (CI starts one; see
    /// docs/meilisearch.md), with MEILI_TEST_KEY as its master key. Skipped otherwise.
    #[test]
    fn meilisearch_agrees_with_sqlite() {
        let Ok(url) = std::env::var("MEILI_TEST_URL") else {
            eprintln!("MEILI_TEST_URL not set; skipping the Meilisearch round trip");
            return;
        };
        let index = format!(
            "test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        );
        let meili = Meili::new(&url, std::env::var("MEILI_TEST_KEY").ok(), &index, 1000);
        let mut conn = db();
        // No index yet: the check builds one and only then lets searches use it
        assert!(!meili.is_ready());
        index::check(&mut conn, &meili).unwrap();
        assert!(meili.is_ready());
        assert_eq!(meili.document_count().unwrap(), Some(6));

        let cases = [
            query(Some("dragon"), None, None, None, None),
            query(Some("Dragon Show"), None, None, None, Some("asc")),
            query(Some("1080p"), Some("1_0"), None, None, None),
            query(Some("1080p"), Some("1_2"), None, None, None),
            query(Some("1080p"), None, Some("1"), None, None),
            query(Some("dragon"), None, Some("2"), None, None),
            query(Some("dragon"), None, Some("3"), None, None),
            query(Some("1080p"), None, None, Some("seeders"), None),
            query(Some("1080p"), None, None, Some("size"), Some("asc")),
            query(None, None, None, Some("seeders"), Some("desc")),
            query(None, None, None, Some("downloads"), Some("asc")),
            query(Some("nothing-matches-this"), None, None, None, None),
        ];
        for q in &cases {
            assert_eq!(ids(&mut conn, Some(&meili), q), ids(&mut conn, None, q), "{q:?}");
        }
        for (q, want) in operator_cases() {
            let total = want.len() as i64;
            assert_eq!(ids(&mut conn, Some(&meili), &q), (want, total), "{q:?}");
        }
        // Spot checks, so the comparison above can't pass with both sides wrong
        assert_eq!(ids(&mut conn, Some(&meili), &cases[0]), (vec![6, 2, 1], 3));
        assert_eq!(ids(&mut conn, Some(&meili), &cases[7]), (vec![3, 6, 1], 3));

        // Moderators see hidden and deleted torrents; profiles hide anonymous ones
        let mut q = query(Some("dragon"), None, None, None, None);
        q.include_deleted = true;
        q.include_hidden = true;
        assert_eq!(ids(&mut conn, Some(&meili), &q), (vec![6, 5, 4, 2, 1], 5));
        let mut q = query(Some("dragon"), None, None, None, None);
        q.user_id = Some(2);
        q.hide_anonymous = true;
        assert_eq!(ids(&mut conn, Some(&meili), &q), (vec![], 0));

        // Counts stop at the hit cap, where Meilisearch stops paging
        let capped = Meili::new(&url, std::env::var("MEILI_TEST_KEY").ok(), &index, 2);
        capped.set_ready(true);
        assert_eq!(ids(&mut conn, Some(&capped), &cases[0]), (vec![6, 2, 1], 2));

        // Paging
        let mut q = query(Some("dragon"), None, None, None, None);
        q.per_page = 2;
        q.page = 2;
        assert_eq!(ids(&mut conn, Some(&meili), &q), (vec![1], 3));

        let wait_for = |conn: &mut DbConnection, q: &SearchQuery, want: Vec<i32>| {
            for _ in 0..100 {
                if ids(conn, Some(&meili), q).0 == want {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("index never showed {want:?} for {q:?}");
        };

        // An edit (here a rename) reaches the index
        diesel::sql_query("UPDATE nyaa_torrents SET display_name = 'Renamed Tale' WHERE id = 1")
            .execute(&mut conn)
            .unwrap();
        index::torrent_changed(&mut conn, Some(&meili), 1);
        wait_for(&mut conn, &query(Some("renamed"), None, None, None, None), vec![1]);

        // Changed tracker stats reach the index, and only those after `since` are sent
        let since = index::sync_stats(&mut conn, &meili, None).unwrap();
        diesel::sql_query(
            "UPDATE nyaa_statistics SET seed_count = 1000, last_updated = '2999-01-01 00:00:00' WHERE torrent_id = 3",
        )
        .execute(&mut conn)
        .unwrap();
        let newest = index::sync_stats(&mut conn, &meili, since).unwrap();
        assert_eq!(newest.unwrap().to_string(), "2999-01-01 00:00:00");
        wait_for(&mut conn, &query(Some("tale"), None, None, Some("seeders"), None), vec![3, 1]);

        // A torrent that never reached the index (uploaded while it was down, or before it
        // was set up) gets the index rebuilt, instead of being missing from every search
        diesel::sql_query(
            "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, uploader_id, \
                           main_category_id, sub_category_id) VALUES (7, X'07', 'Display Name2', 't', 0, 1, 1, 1)",
        )
        .execute(&mut conn)
        .unwrap();
        diesel::sql_query(
            "INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) VALUES (7, 1, 0, 0)",
        )
        .execute(&mut conn)
        .unwrap();
        // Stats sync sends whole documents, so it adds the torrent rather than a nameless stub
        index::sync_stats(&mut conn, &meili, None).unwrap();
        wait_for(&mut conn, &query(Some("Name2"), None, None, None, None), vec![7]);
        // Digits inside a word match on their own, as do numbers without leading zeros ("02")
        assert_eq!(ids(&mut conn, Some(&meili), &query(Some("2"), None, None, None, None)), (vec![7, 2], 2));
        // One with no stats change never reaches it that way; the count check catches it
        diesel::sql_query(
            "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, uploader_id, \
                           main_category_id, sub_category_id) VALUES (8, X'08', 'Display Name3', 't', 0, 1, 1, 1)",
        )
        .execute(&mut conn)
        .unwrap();
        while meili.has_pending_tasks().unwrap() {
            std::thread::sleep(Duration::from_millis(20));
        }
        index::check(&mut conn, &meili).unwrap();
        assert!(meili.is_ready());
        assert_eq!(ids(&mut conn, Some(&meili), &query(Some("display"), None, None, None, None)), (vec![8, 7], 2));

        // An index with other settings (built by an older version) is rebuilt
        assert!(meili.settings_current().unwrap());
        let mut old = meili::index_settings(1000);
        old["searchableAttributes"] = serde_json::json!(["display_name"]);
        let req = ureq::patch(&format!("{url}/indexes/{index}/settings"))
            .header("Authorization", &format!("Bearer {}", std::env::var("MEILI_TEST_KEY").unwrap_or_default()));
        req.send_json(&old).unwrap();
        while meili.has_pending_tasks().unwrap() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!meili.settings_current().unwrap());
        index::check(&mut conn, &meili).unwrap();
        assert!(meili.settings_current().unwrap() && meili.is_ready());

        meili.delete_index().unwrap();
    }
}
