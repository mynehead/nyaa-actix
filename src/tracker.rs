//! The torrust-actix tracker's management API (optional, TRACKER_API_URL): keeps the
//! tracker's whitelist in step with the site and pulls seeders, leechers and completed
//! counts into `nyaa_statistics`.
//!
//! Upstream nyaa queues whitelist changes for its tracker in a table. Here handlers hand the
//! ids of changed torrents to a background thread instead, which sends them in batches. That
//! thread also sends the whole whitelist whenever the tracker (re)starts or a call failed, so
//! a change made while the tracker was down still reaches it, and an upload never waits on it.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use diesel::prelude::*;
use serde_json::{json, Value};

use crate::db::schema::{nyaa_statistics, nyaa_torrents};
use crate::db::{DbConnection, DbPool};
use crate::models::{NewStatistic, Statistic, TorrentFlags};

/// Hashes per whitelist call; the tracker refuses bodies over 1 MiB (43 bytes per hash).
const WHITELIST_BATCH: usize = 1000;
/// Hashes per stats call. Its answer lists every peer of each torrent, so keep it small.
const STATS_BATCH: usize = 200;

/// Torrents the tracker should track: everything not deleted or banned (upstream removes
/// those two from its tracker; hidden torrents stay announceable).
fn listed(flags: i32) -> bool {
    flags & (TorrentFlags::DELETED | TorrentFlags::BANNED).bits() == 0
}

#[derive(Clone)]
pub struct Tracker {
    api: Api,
    changes: Sender<i32>,
    /// Taken by `spawn_sync`; until then changes queue up in the channel.
    receiver: Arc<Mutex<Option<Receiver<i32>>>>,
}

impl fmt::Debug for Tracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not the key
        f.debug_struct("Tracker").field("url", &self.api.url).finish()
    }
}

impl Tracker {
    pub fn new(url: &str, key: &str) -> Self {
        let (changes, receiver) = mpsc::channel();
        Tracker { api: Api::new(url, key), changes, receiver: Arc::new(Mutex::new(Some(receiver))) }
    }

    /// Reads TRACKER_API_URL and TRACKER_API_KEY. None when TRACKER_API_URL is unset or
    /// empty: the site then runs without talking to a tracker.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("TRACKER_API_URL").ok().filter(|u| !u.trim().is_empty())?;
        Some(Tracker::new(url.trim(), std::env::var("TRACKER_API_KEY").unwrap_or_default().trim()))
    }

    pub fn url(&self) -> &str {
        &self.api.url
    }
}

/// Call after a torrent was uploaded, deleted, banned or restored. Only queues the id; the
/// sync thread adds the torrent to the whitelist or removes it, whichever its flags say.
pub fn torrent_changed(tracker: Option<&Tracker>, id: i32) {
    if let Some(tracker) = tracker {
        // Fails only once the sync thread is gone, and then there is no one left to tell
        let _ = tracker.changes.send(id);
    }
}

/// Starts the thread that applies whitelist changes as they come and syncs stats `every`.
pub fn spawn_sync(pool: DbPool, tracker: Tracker, every: Duration) {
    let Some(receiver) = tracker.receiver.lock().unwrap().take() else { return };
    std::thread::spawn(move || {
        let api = tracker.api;
        // The tracker's start time when the whole whitelist was last sent; None sends it again
        let mut synced_start: Option<i64> = None;
        let mut next_stats = Instant::now();
        loop {
            let mut changed = HashSet::new();
            match receiver.recv_timeout(next_stats.saturating_duration_since(Instant::now())) {
                Ok(id) => {
                    changed.insert(id);
                    changed.extend(receiver.try_iter());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(next_stats.saturating_duration_since(Instant::now()))
                }
            }
            let stats_due = Instant::now() >= next_stats;
            if stats_due {
                next_stats = Instant::now() + every;
            }
            let result = pool.get().map_err(anyhow::Error::from).and_then(|mut conn| {
                // Before the first full sync (or after a failure) the next one covers these
                if !changed.is_empty() && synced_start.is_some() {
                    let ids: Vec<i32> = changed.into_iter().collect();
                    push_whitelist(&mut conn, &api, Some(&ids))?;
                }
                if stats_due {
                    // A restarted tracker without a database has forgotten the whitelist
                    let started = api.started()?;
                    if synced_start != Some(started) {
                        let count = push_whitelist(&mut conn, &api, None)?;
                        log::info!("Sent {count} torrents to the tracker's whitelist");
                        synced_start = Some(started);
                    }
                    sync_stats(&mut conn, &api)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                log::warn!("Tracker API at {} is not usable ({e:#}); retrying in {} s", api.url, every.as_secs());
                synced_start = None;
            }
        }
    });
}

/// Adds the listed torrents among `ids` (all torrents for None) to the whitelist and
/// removes the others. Returns how many it added.
fn push_whitelist(conn: &mut DbConnection, api: &Api, ids: Option<&[i32]>) -> anyhow::Result<usize> {
    let mut added = 0;
    let mut last_id = i32::MIN;
    loop {
        let mut query = nyaa_torrents::table
            .select((nyaa_torrents::id, nyaa_torrents::info_hash, nyaa_torrents::flags))
            .filter(nyaa_torrents::id.gt(last_id))
            .order(nyaa_torrents::id.asc())
            .limit(WHITELIST_BATCH as i64)
            .into_boxed();
        if let Some(ids) = ids {
            query = query.filter(nyaa_torrents::id.eq_any(ids));
        }
        let rows: Vec<(i32, Vec<u8>, i32)> = query.load(conn)?;
        let Some(&(last, ..)) = rows.last() else { break };
        last_id = last;
        let (add, remove): (Vec<_>, Vec<_>) = rows.iter().partition(|(_, _, flags)| listed(*flags));
        let hex =
            |rows: Vec<&(i32, Vec<u8>, i32)>| rows.into_iter().map(|(_, h, _)| hex::encode(h)).collect::<Vec<_>>();
        added += add.len();
        api.whitelist(true, &hex(add))?;
        api.whitelist(false, &hex(remove))?;
    }
    Ok(added)
}

/// Pulls the counts of every listed torrent from the tracker. Seeders and leechers are
/// replaced; download_count grows by the downloads the tracker counted since the last sync.
/// Rows whose numbers changed get a new `last_updated`, which the Meilisearch sync picks up.
fn sync_stats(conn: &mut DbConnection, api: &Api) -> anyhow::Result<()> {
    let mut last_id = i32::MIN;
    loop {
        let rows: Vec<(i32, Vec<u8>, i32)> = nyaa_torrents::table
            .select((nyaa_torrents::id, nyaa_torrents::info_hash, nyaa_torrents::flags))
            .filter(nyaa_torrents::id.gt(last_id))
            .order(nyaa_torrents::id.asc())
            .limit(STATS_BATCH as i64)
            .load(conn)?;
        let Some(&(last, ..)) = rows.last() else { break };
        last_id = last;
        // Deleted and banned torrents aren't on the tracker; their counts stay as they are
        let rows: Vec<(i32, Vec<u8>)> =
            rows.into_iter().filter(|(_, _, flags)| listed(*flags)).map(|(id, h, _)| (id, h)).collect();
        if rows.is_empty() {
            continue;
        }
        let hashes: Vec<String> = rows.iter().map(|(_, h)| hex::encode(h)).collect();
        let swarms = api.swarms(&hashes)?;
        let ids: Vec<i32> = rows.iter().map(|(id, _)| *id).collect();
        let old: HashMap<i32, Statistic> = nyaa_statistics::table
            .filter(nyaa_statistics::torrent_id.eq_any(&ids))
            .load::<Statistic>(conn)?
            .into_iter()
            .map(|s| (s.torrent_id, s))
            .collect();
        let now = chrono::Utc::now().naive_utc();
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            for (id, hash) in ids.iter().zip(&hashes) {
                let swarm = swarms.get(hash).copied();
                let Some(old) = old.get(id) else {
                    // Every upload gets a row; make one for any torrent that somehow has none
                    let (seed_count, leech_count, completed) = swarm.unwrap_or_default();
                    diesel::insert_into(nyaa_statistics::table)
                        .values(&NewStatistic {
                            torrent_id: *id,
                            seed_count,
                            leech_count,
                            download_count: completed,
                            last_updated: now,
                        })
                        .execute(conn)?;
                    diesel::update(nyaa_statistics::table.find(id))
                        .set(nyaa_statistics::tracker_completed.eq(completed))
                        .execute(conn)?;
                    continue;
                };
                let new = updated_stats(old, swarm);
                if new != (old.seed_count, old.leech_count, old.download_count, old.tracker_completed) {
                    diesel::update(nyaa_statistics::table.find(id))
                        .set((
                            nyaa_statistics::seed_count.eq(new.0),
                            nyaa_statistics::leech_count.eq(new.1),
                            nyaa_statistics::download_count.eq(new.2),
                            nyaa_statistics::tracker_completed.eq(new.3),
                            nyaa_statistics::last_updated.eq(now),
                        ))
                        .execute(conn)?;
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// What the tracker reports for one torrent: (seeders, leechers, completed).
type Swarm = (i32, i32, i32);

/// The new stats row for a torrent the tracker reports as `swarm` (None when it doesn't
/// know the torrent, which for a tracker without a database means no peers are left and its
/// completed count is gone with them). `old` is the row from the last sync.
fn updated_stats(old: &Statistic, swarm: Option<Swarm>) -> (i32, i32, i32, i32) {
    let (seeders, leechers, completed) = swarm.unwrap_or((0, 0, 0));
    // A count below the last one means the tracker started over from zero
    let new_downloads = if completed >= old.tracker_completed { completed - old.tracker_completed } else { completed };
    (seeders, leechers, old.download_count.saturating_add(new_downloads), completed)
}

#[derive(Clone)]
struct Api {
    url: String,
    key: String,
    agent: ureq::Agent,
}

impl Api {
    fn new(url: &str, key: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            // Error bodies carry the tracker's reason; read them instead of dropping them
            .http_status_as_error(false)
            .build()
            .into();
        Api { url: url.trim_end_matches('/').to_string(), key: key.to_string(), agent }
    }

    /// Calls the API and returns its JSON answer. The tracker answers some refusals (a
    /// bad token, an address it doesn't serve) with 200, so `status` must say ok as well.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let url = format!("{}{}", self.url, path);
        let auth = format!("Bearer {}", self.key);
        let mut resp = match (method, body) {
            ("GET", None) => self.agent.get(&url).header("Authorization", &auth).call(),
            // The tracker reads the hash list of these GETs and DELETEs from the body
            ("GET", Some(b)) => self.agent.get(&url).header("Authorization", &auth).force_send_body().send_json(b),
            ("DELETE", Some(b)) => {
                self.agent.delete(&url).header("Authorization", &auth).force_send_body().send_json(b)
            }
            ("POST", Some(b)) => self.agent.post(&url).header("Authorization", &auth).send_json(b),
            _ => unreachable!("{method} {path}"),
        }
        .with_context(|| format!("{method} {path}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string()?;
        let value: Value = serde_json::from_str(&text).map_err(|_| anyhow!("{method} {path}: {status} {text}"))?;
        if !(200..300).contains(&status) || value["status"] != "ok" && path != "/stats" {
            bail!("{method} {path}: {status} {text}");
        }
        Ok(value)
    }

    /// When the tracker process started (unix seconds), from `GET /stats`.
    fn started(&self) -> anyhow::Result<i64> {
        let v = self.call("GET", "/stats", None)?;
        v["started"].as_i64().ok_or_else(|| anyhow!("GET /stats: no start time in {v}"))
    }

    /// `POST /api/whitelists` (add) or `DELETE /api/whitelists` (remove) for hex hashes.
    fn whitelist(&self, add: bool, hashes: &[String]) -> anyhow::Result<()> {
        if !hashes.is_empty() {
            self.call(if add { "POST" } else { "DELETE" }, "/api/whitelists", Some(&json!(hashes)))?;
        }
        Ok(())
    }

    /// `GET /api/torrents` for hex hashes: the swarm of each one the tracker knows.
    fn swarms(&self, hashes: &[String]) -> anyhow::Result<HashMap<String, Swarm>> {
        let v = self.call("GET", "/api/torrents", Some(&json!(hashes)))?;
        let torrents = v["torrents"].as_object().ok_or_else(|| anyhow!("GET /api/torrents: {v}"))?;
        let count = |t: &Value, key: &str| t[key].as_array().map_or(0, |a| a.len() as i32);
        Ok(torrents
            .iter()
            .map(|(hash, t)| {
                let completed = t["completed"].as_u64().unwrap_or(0).min(i32::MAX as u64) as i32;
                (hash.to_lowercase(), (count(t, "seeds"), count(t, "peers"), completed))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn stats(download_count: i32, tracker_completed: i32) -> Statistic {
        Statistic {
            torrent_id: 1,
            seed_count: 5,
            leech_count: 5,
            download_count,
            last_updated: NaiveDateTime::default(),
            tracker_completed,
        }
    }

    #[test]
    fn downloads_grow_by_what_the_tracker_counted_since_last_time() {
        // First sync of a new upload
        assert_eq!(updated_stats(&stats(0, 0), Some((2, 3, 4))), (2, 3, 4, 4));
        // Three more downloads
        assert_eq!(updated_stats(&stats(4, 4), Some((1, 0, 7))), (1, 0, 7, 7));
        // The tracker restarted and has counted two since
        assert_eq!(updated_stats(&stats(7, 7), Some((1, 1, 2))), (1, 1, 9, 2));
        // The tracker forgot the torrent (no peers left): nothing is lost, counting restarts
        assert_eq!(updated_stats(&stats(9, 2), None), (0, 0, 9, 0));
    }

    #[test]
    fn deleted_and_banned_torrents_are_not_listed() {
        assert!(listed(0));
        assert!(listed((TorrentFlags::HIDDEN | TorrentFlags::REMAKE).bits()));
        assert!(!listed(TorrentFlags::DELETED.bits()));
        assert!(!listed(TorrentFlags::BANNED.bits()));
    }

    /// Against a real torrust-actix with `TRACKER__WHITELIST_ENABLED=true`: TRACKER_TEST_URL
    /// (API), TRACKER_TEST_KEY and TRACKER_TEST_ANNOUNCE_URL (its HTTP announce URL).
    /// Skipped when they are unset; CI starts one.
    #[test]
    fn whitelist_and_stats_round_trip() {
        let (Ok(url), Ok(key), Ok(announce)) = (
            std::env::var("TRACKER_TEST_URL"),
            std::env::var("TRACKER_TEST_KEY"),
            std::env::var("TRACKER_TEST_ANNOUNCE_URL"),
        ) else {
            eprintln!("TRACKER_TEST_URL, TRACKER_TEST_KEY or TRACKER_TEST_ANNOUNCE_URL not set; skipping");
            return;
        };
        let api = Api::new(&url, &key);
        let mut conn = crate::db::connect(":memory:").unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        // Hashes unique to this run, so a tracker reused between runs starts clean
        let salt = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let hash = |id: i32| format!("{:08x}{salt:032x}", id);
        for (id, flags) in [(1, 0), (2, TorrentFlags::DELETED.bits()), (3, TorrentFlags::HIDDEN.bits())] {
            diesel::sql_query(format!(
                "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, flags, \
                 main_category_id, sub_category_id, filesize) VALUES ({id}, X'{}', 't{id}', 't', {flags}, 1, 2, 1)",
                hash(id)
            ))
            .execute(&mut conn)
            .unwrap();
        }
        // Torrent 3 has no stats row yet; the sync makes one
        diesel::sql_query("INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) VALUES (1, 0, 0, 0), (2, 0, 0, 0)")
            .execute(&mut conn)
            .unwrap();
        let whitelisted = |ids: &[i32]| -> Vec<bool> {
            let hashes: Vec<String> = ids.iter().map(|&id| hash(id)).collect();
            let v = api.call("GET", "/api/whitelists", Some(&json!(hashes))).unwrap();
            hashes.iter().map(|h| v["whitelists"][h].as_bool().unwrap()).collect()
        };

        assert_eq!(push_whitelist(&mut conn, &api, None).unwrap(), 2);
        assert_eq!(whitelisted(&[1, 2, 3]), [true, false, true]);

        // A seeder that just finished and a leecher on torrent 1
        let peer = |n: u8, left: u64, event: &str| {
            let info_hash: String = hex::decode(hash(1)).unwrap().iter().map(|b| format!("%{b:02X}")).collect();
            let peer_id = format!("-NY0001-{n:012}");
            let url = format!(
                "{announce}?info_hash={info_hash}&peer_id={peer_id}&port={}&uploaded=0&downloaded=0&left={left}&compact=1{event}",
                6880 + n as u16
            );
            let body = ureq::get(&url).call().unwrap().body_mut().read_to_string().unwrap();
            assert!(!body.contains("failure"), "{body}");
        };
        peer(1, 0, "&event=completed");
        peer(2, 100, "&event=started");

        sync_stats(&mut conn, &api).unwrap();
        let row = |conn: &mut DbConnection, id: i32| -> (i32, i32, i32, i32) {
            nyaa_statistics::table
                .find(id)
                .select((
                    nyaa_statistics::seed_count,
                    nyaa_statistics::leech_count,
                    nyaa_statistics::download_count,
                    nyaa_statistics::tracker_completed,
                ))
                .first(conn)
                .unwrap()
        };
        assert_eq!(row(&mut conn, 1), (1, 1, 1, 1));
        assert_eq!(row(&mut conn, 3), (0, 0, 0, 0));
        // Counted once, not again on the next sync
        sync_stats(&mut conn, &api).unwrap();
        assert_eq!(row(&mut conn, 1), (1, 1, 1, 1));

        // Deleting torrent 3 takes it off the whitelist
        diesel::update(nyaa_torrents::table.find(3))
            .set(nyaa_torrents::flags.eq(TorrentFlags::DELETED.bits()))
            .execute(&mut conn)
            .unwrap();
        assert_eq!(push_whitelist(&mut conn, &api, Some(&[3])).unwrap(), 0);
        assert_eq!(whitelisted(&[1, 3]), [true, false]);
        assert!(api.started().unwrap() > 0);
    }
}
