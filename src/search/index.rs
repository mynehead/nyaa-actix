//! Keeping the Meilisearch index in step with SQLite: single torrents after an upload or
//! edit, tracker stats on a timer, and full rebuilds from the `reindex` subcommand.

use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use diesel::prelude::*;

use super::meili::{Meili, TorrentDoc};
use crate::db::schema::{nyaa_statistics, nyaa_torrents};
use crate::db::{DbConnection, DbPool};
use crate::models::{Statistic, Torrent};

/// Documents per indexing request.
const BATCH: i64 = 10_000;
/// How long a rebuild waits for Meilisearch to process one batch or settings change.
const TASK_TIMEOUT: Duration = Duration::from_secs(600);

fn docs_for(conn: &mut DbConnection, torrents: &[Torrent]) -> QueryResult<Vec<TorrentDoc>> {
    let ids: Vec<i32> = torrents.iter().map(|t| t.id).collect();
    let stats: std::collections::HashMap<i32, Statistic> = nyaa_statistics::table
        .filter(nyaa_statistics::torrent_id.eq_any(&ids))
        .load::<Statistic>(conn)?
        .into_iter()
        .map(|s| (s.torrent_id, s))
        .collect();
    Ok(torrents.iter().map(|t| TorrentDoc::new(t, stats.get(&t.id))).collect())
}

/// Pushes the current state of one torrent to the index, if there is one. Search falls
/// back to SQLite when the index misbehaves, so a failure here is logged, not returned:
/// the upload or edit itself already succeeded.
pub fn torrent_changed(conn: &mut DbConnection, meili: Option<&Meili>, id: i32) {
    let Some(meili) = meili else { return };
    let result = (|| -> anyhow::Result<()> {
        let Some(t) = Torrent::by_id(conn, id)? else { return Ok(()) };
        meili.put_documents(&docs_for(conn, &[t])?)?;
        Ok(())
    })();
    if let Err(e) = result {
        log::warn!("Could not index torrent #{id}, searching SQLite until the index is rebuilt: {e:#}");
        meili.mark_stale();
    }
}

/// Builds a complete index from SQLite under a temporary name, then swaps it in, so
/// searches keep working on the old one meanwhile. Returns how many torrents it indexed.
pub fn rebuild(conn: &mut DbConnection, meili: &Meili, progress: impl Fn(i64)) -> anyhow::Result<i64> {
    let fresh = meili.with_index(&format!("{}_rebuild", meili.index()));
    // Left over from an interrupted run; the task fails when there is none, which is fine
    if let Ok(uid) = fresh.delete_index() {
        let _ = fresh.wait(uid, TASK_TIMEOUT);
    }
    fresh.wait(fresh.configure()?, TASK_TIMEOUT)?;

    let mut last_id = 0;
    let mut count = 0;
    let mut pending = None;
    loop {
        let torrents: Vec<Torrent> = nyaa_torrents::table
            .filter(nyaa_torrents::id.gt(last_id))
            .order(nyaa_torrents::id.asc())
            .limit(BATCH)
            .load(conn)?;
        let Some(last) = torrents.last() else { break };
        last_id = last.id;
        let docs = docs_for(conn, &torrents)?;
        // Keep one batch in flight while reading the next from SQLite
        if let Some(uid) = pending.take() {
            fresh.wait(uid, TASK_TIMEOUT)?;
        }
        pending = Some(fresh.put_documents(&docs)?);
        count += docs.len() as i64;
        progress(count);
    }
    if let Some(uid) = pending {
        fresh.wait(uid, TASK_TIMEOUT)?;
    }

    // The live index must exist to swap with; afterwards the fresh name holds the old data
    meili.wait(meili.configure()?, TASK_TIMEOUT)?;
    meili.wait(meili.swap_with(fresh.index())?, TASK_TIMEOUT)?;
    fresh.wait(fresh.delete_index()?, TASK_TIMEOUT)?;
    Ok(count)
}

/// Makes sure the index can be trusted before searches use it: rebuilds it when it is
/// missing, holds a different number of torrents than the database (say it was set up
/// after torrents were uploaded, or an upload happened while it was down), or an update
/// failed to reach it. Then marks it ready.
pub fn check(conn: &mut DbConnection, meili: &Meili) -> anyhow::Result<()> {
    // Counts lag behind queued updates; look again next time
    if meili.has_pending_tasks()? {
        return Ok(());
    }
    let stale = meili.take_stale();
    let torrents: i64 = nyaa_torrents::table.count().get_result(conn)?;
    let indexed = meili.document_count()?;
    if !stale && indexed == Some(torrents) {
        if !meili.is_ready() {
            log::info!("Meilisearch index `{}` holds all {torrents} torrents; searching with it", meili.index());
        }
        meili.set_ready(true);
        return Ok(());
    }
    meili.set_ready(false);
    log::info!(
        "Rebuilding Meilisearch index `{}` ({} of {torrents} torrents indexed{}); searching SQLite meanwhile",
        meili.index(),
        indexed.map_or("none".into(), |n| n.to_string()),
        if stale { ", an update failed" } else { "" },
    );
    let start = Instant::now();
    let count = rebuild(conn, meili, |_| {})?;
    log::info!("Indexed {count} torrents in {:.1?}", start.elapsed());
    // An update that failed while rebuilding means another round
    if !meili.take_stale() {
        meili.set_ready(true);
    } else {
        meili.mark_stale();
    }
    Ok(())
}

/// Pushes the torrents whose tracker stats changed at or after `since` (all of them for
/// None) and returns the newest `last_updated` seen, to pass as `since` next time. Whole
/// documents go out, so a torrent missing from the index is added rather than left as a
/// nameless stats-only entry.
pub fn sync_stats(
    conn: &mut DbConnection,
    meili: &Meili,
    since: Option<NaiveDateTime>,
) -> anyhow::Result<Option<NaiveDateTime>> {
    let mut newest = since;
    let mut last_id = 0;
    loop {
        let mut query = nyaa_statistics::table
            .filter(nyaa_statistics::torrent_id.gt(last_id))
            .order(nyaa_statistics::torrent_id.asc())
            .limit(BATCH)
            .into_boxed();
        // `>=`: rows written later in the same second as the last sync still go out
        if let Some(t) = since {
            query = query.filter(nyaa_statistics::last_updated.ge(t));
        }
        let rows: Vec<Statistic> = query.load(conn)?;
        let Some(last) = rows.last() else { break };
        last_id = last.torrent_id;
        newest = rows.iter().map(|s| s.last_updated).chain(newest).max();
        let ids: Vec<i32> = rows.iter().map(|s| s.torrent_id).collect();
        let torrents: Vec<Torrent> = nyaa_torrents::table.filter(nyaa_torrents::id.eq_any(&ids)).load(conn)?;
        let stats: std::collections::HashMap<i32, &Statistic> = rows.iter().map(|s| (s.torrent_id, s)).collect();
        let docs: Vec<TorrentDoc> = torrents.iter().map(|t| TorrentDoc::new(t, stats.get(&t.id).copied())).collect();
        if !docs.is_empty() {
            meili.put_documents(&docs)?;
        }
    }
    Ok(newest)
}

/// Runs on a background thread for the life of the server: checks the index (building it
/// when needed) and keeps seed/leech/download counts current for sorting.
pub fn spawn_stats_sync(pool: DbPool, meili: Meili, every: Duration) {
    std::thread::spawn(move || {
        let mut since = None;
        loop {
            let started = Instant::now();
            let result = pool.get().map_err(anyhow::Error::from).and_then(|mut conn| {
                check(&mut conn, &meili)?;
                if meili.is_ready() {
                    since = sync_stats(&mut conn, &meili, since)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                log::warn!("Meilisearch at {meili:?} is not usable ({e:#}); searching SQLite until it is");
                meili.set_ready(false);
            }
            std::thread::sleep(every.saturating_sub(started.elapsed()));
        }
    });
}
