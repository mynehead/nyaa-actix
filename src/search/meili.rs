//! Optional Meilisearch index of torrents, in the role upstream gives Elasticsearch.
//!
//! SQLite stays the source of truth: the index only picks which torrent ids match and in
//! what order, and the rows themselves are loaded from SQLite. Uploads and edits push the
//! changed torrent, a background task pushes changed tracker stats, and `nyaa-actix reindex`
//! rebuilds the whole index. Talks to Meilisearch's HTTP API directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use super::db::{SearchOrder, SearchQuery, SearchSort};
use crate::models::{Statistic, Torrent, TorrentFlags};

/// How long a listing waits for Meilisearch before falling back to SQLite.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(2);

/// A Meilisearch server and the index torrents live in.
#[derive(Clone)]
pub struct Meili {
    url: String,
    key: Option<String>,
    index: String,
    /// Upper bound on how many hits a search counts and pages through.
    max_hits: i64,
    agent: ureq::Agent,
    /// Whether the index is known to hold every torrent. Until it is (and whenever an
    /// update fails to reach it), listings search SQLite instead; see `index::check`.
    ready: Arc<AtomicBool>,
    /// Set when an update failed to reach the index, so the next check rebuilds it.
    stale: Arc<AtomicBool>,
}

impl std::fmt::Debug for Meili {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Leaves the key out of logs
        f.debug_struct("Meili").field("url", &self.url).field("index", &self.index).finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MeiliError {
    #[error("Meilisearch request failed: {0}")]
    Http(#[from] ureq::Error),
    #[error("Meilisearch answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("Meilisearch task {uid} {status}: {error}")]
    Task { uid: u64, status: String, error: String },
    #[error("unexpected Meilisearch response: {0}")]
    Response(String),
}

pub type MeiliResult<T> = Result<T, MeiliError>;

/// Settings the searches below rely on. Upstream's listing order (newest first, or the
/// chosen sort column) beats relevance, so `sort` ranks first; every word must match, as
/// with Elasticsearch's AND operator; and there is no typo tolerance, since release names
/// that differ by a letter are usually different releases.
pub fn index_settings(max_hits: i64) -> Value {
    json!({
        "searchableAttributes": ["display_name"],
        "filterableAttributes": [
            "main_category_id", "sub_category_id", "uploader_id", "group_id",
            "hidden", "anonymous", "remake", "trusted", "complete", "deleted"
        ],
        "sortableAttributes": [
            "id", "display_name", "filesize", "comment_count",
            "seed_count", "leech_count", "download_count"
        ],
        "rankingRules": ["sort", "words", "typo", "proximity", "attribute", "exactness"],
        "typoTolerance": { "enabled": false },
        "pagination": { "maxTotalHits": max_hits },
    })
}

/// One torrent as indexed: the searchable name plus everything listings filter or sort on.
#[derive(Debug, Serialize, PartialEq)]
pub struct TorrentDoc {
    pub id: i32,
    pub display_name: String,
    pub main_category_id: i32,
    pub sub_category_id: i32,
    pub uploader_id: Option<i32>,
    pub group_id: Option<i32>,
    pub filesize: i64,
    pub comment_count: i32,
    pub seed_count: i32,
    pub leech_count: i32,
    pub download_count: i32,
    pub hidden: bool,
    pub anonymous: bool,
    pub remake: bool,
    pub trusted: bool,
    pub complete: bool,
    /// Deleted or banned: listings drop both unless a moderator is looking.
    pub deleted: bool,
}

impl TorrentDoc {
    pub fn new(t: &Torrent, stats: Option<&Statistic>) -> Self {
        let has = |flag: TorrentFlags| t.flags & flag.bits() != 0;
        TorrentDoc {
            id: t.id,
            display_name: t.display_name.clone(),
            main_category_id: t.main_category_id,
            sub_category_id: t.sub_category_id,
            uploader_id: t.uploader_id,
            group_id: t.group_id,
            filesize: t.filesize,
            comment_count: t.comment_count,
            seed_count: stats.map_or(0, |s| s.seed_count),
            leech_count: stats.map_or(0, |s| s.leech_count),
            download_count: stats.map_or(0, |s| s.download_count),
            hidden: has(TorrentFlags::HIDDEN),
            anonymous: has(TorrentFlags::ANONYMOUS),
            remake: has(TorrentFlags::REMAKE),
            trusted: has(TorrentFlags::TRUSTED),
            complete: has(TorrentFlags::COMPLETE),
            deleted: has(TorrentFlags::DELETED) || has(TorrentFlags::BANNED),
        }
    }
}

/// The Meilisearch filter expression for a listing, mirroring `db::filtered`.
pub fn filter(q: &SearchQuery) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(uid) = q.user_id {
        f.push(format!("uploader_id = {uid}"));
    }
    if let Some(gid) = q.group_id {
        f.push(format!("group_id = {gid}"));
    }
    if let Some(main) = q.main_category {
        f.push(format!("main_category_id = {main}"));
        if let Some(sub) = q.sub_category {
            f.push(format!("sub_category_id = {sub}"));
        }
    }
    match q.quality_filter {
        1 => f.push("remake = false".into()),
        2 => f.push("trusted = true".into()),
        3 => f.push("trusted = true AND complete = true".into()),
        _ => {}
    }
    if !q.include_deleted {
        f.push("deleted = false".into());
    }
    if !q.include_hidden {
        f.push("hidden = false".into());
    }
    if q.hide_anonymous {
        f.push("anonymous = false".into());
    }
    f
}

/// The sort for a listing, newest first among ties as in SQL.
pub fn sort(q: &SearchQuery) -> Vec<String> {
    let field = match q.sort {
        SearchSort::Id => "id",
        SearchSort::Name => "display_name",
        SearchSort::Size => "filesize",
        SearchSort::Seeders => "seed_count",
        SearchSort::Leechers => "leech_count",
        SearchSort::Downloads => "download_count",
        SearchSort::Comments => "comment_count",
    };
    let dir = match q.order { SearchOrder::Asc => "asc", SearchOrder::Desc => "desc" };
    let mut s = vec![format!("{field}:{dir}")];
    if field != "id" {
        s.push("id:desc".into());
    }
    s
}

/// The body of a search request for one page of a listing.
pub fn search_body(q: &SearchQuery) -> Value {
    json!({
        "q": q.term.as_deref().unwrap_or(""),
        "filter": filter(q),
        "sort": sort(q),
        "page": q.page,
        "hitsPerPage": q.per_page,
        "attributesToRetrieve": ["id"],
        "matchingStrategy": "all",
    })
}

impl Meili {
    pub fn new(url: &str, key: Option<String>, index: &str, max_hits: i64) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            // Error bodies carry Meilisearch's explanation; read them instead of dropping them
            .http_status_as_error(false)
            .build()
            .into();
        Meili {
            url: url.trim_end_matches('/').to_string(),
            key: key.filter(|k| !k.is_empty()),
            index: index.to_string(),
            max_hits,
            agent,
            ready: Arc::new(AtomicBool::new(false)),
            stale: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Reads MEILI_URL, MEILI_KEY, MEILI_INDEX and MEILI_MAX_HITS. None when MEILI_URL is
    /// unset or empty, which keeps search on SQLite.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("MEILI_URL").ok().filter(|u| !u.trim().is_empty())?;
        let index = std::env::var("MEILI_INDEX").ok().filter(|i| !i.is_empty()).unwrap_or_else(|| "torrents".into());
        let max_hits = std::env::var("MEILI_MAX_HITS").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
        Some(Meili::new(url.trim(), std::env::var("MEILI_KEY").ok(), &index, max_hits))
    }

    pub fn index(&self) -> &str {
        &self.index
    }

    /// The same server, but another index (used to build a fresh one for reindexing).
    pub fn with_index(&self, index: &str) -> Self {
        Meili { index: index.to_string(), ..self.clone() }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> MeiliResult<Value> {
        self.call_within(method, path, body, None)
    }

    fn call_within(&self, method: &str, path: &str, body: Option<&Value>, timeout: Option<Duration>) -> MeiliResult<Value> {
        let url = format!("{}{}", self.url, path);
        let auth = self.key.as_ref().map(|k| format!("Bearer {k}"));
        macro_rules! send {
            ($builder:expr) => {{
                let mut b = $builder;
                if let Some(t) = timeout { b = b.config().timeout_global(Some(t)).build(); }
                if let Some(a) = &auth { b = b.header("Authorization", a); }
                b
            }};
        }
        let mut resp = match (method, body) {
            ("GET", _) => send!(self.agent.get(&url)).call()?,
            ("DELETE", _) => send!(self.agent.delete(&url)).call()?,
            ("POST", Some(b)) => send!(self.agent.post(&url)).send_json(b)?,
            ("PUT", Some(b)) => send!(self.agent.put(&url)).send_json(b)?,
            ("PATCH", Some(b)) => send!(self.agent.patch(&url)).send_json(b)?,
            _ => unreachable!("{method} without a body"),
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string()?;
        if !(200..300).contains(&status) {
            return Err(MeiliError::Status { status, body: text });
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| MeiliError::Response(format!("{e}: {text}")))
    }

    fn task_uid(v: &Value) -> MeiliResult<u64> {
        v["taskUid"].as_u64().ok_or_else(|| MeiliError::Response(v.to_string()))
    }

    /// Torrent ids for one page of a listing, in order, and the number of matches.
    pub fn search(&self, q: &SearchQuery) -> MeiliResult<(Vec<i32>, i64)> {
        // A page waits on this; past the timeout SQLite answers instead
        let r = self.call_within("POST", &format!("/indexes/{}/search", self.index), Some(&search_body(q)), Some(SEARCH_TIMEOUT))?;
        let ids = r["hits"].as_array()
            .ok_or_else(|| MeiliError::Response(r.to_string()))?
            .iter()
            .filter_map(|h| h["id"].as_i64().map(|id| id as i32))
            .collect();
        let total = r["totalHits"].as_i64().ok_or_else(|| MeiliError::Response(r.to_string()))?;
        // Counts go past maxTotalHits but pages there come back empty; don't offer them
        Ok((ids, total.min(self.max_hits)))
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Relaxed);
    }

    /// An update didn't reach the index: search SQLite until it has been rebuilt.
    pub fn mark_stale(&self) {
        self.stale.store(true, Ordering::Relaxed);
        self.set_ready(false);
    }

    /// Whether an update failed since the last call.
    pub fn take_stale(&self) -> bool {
        self.stale.swap(false, Ordering::Relaxed)
    }

    /// How many documents the index holds, or None when it doesn't exist.
    pub fn document_count(&self) -> MeiliResult<Option<i64>> {
        match self.call("GET", &format!("/indexes/{}/stats", self.index), None) {
            Ok(v) => v["numberOfDocuments"].as_i64().map(Some).ok_or_else(|| MeiliError::Response(v.to_string())),
            Err(MeiliError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether any update to the index is still queued or running.
    pub fn has_pending_tasks(&self) -> MeiliResult<bool> {
        let v = self.call("GET", &format!("/tasks?indexUids={}&statuses=enqueued,processing&limit=1", self.index), None)?;
        v["total"].as_u64().map(|t| t > 0).ok_or_else(|| MeiliError::Response(v.to_string()))
    }

    /// Creates the index if needed and applies `index_settings`. Returns the settings task.
    pub fn configure(&self) -> MeiliResult<u64> {
        match self.call("POST", "/indexes", Some(&json!({ "uid": self.index, "primaryKey": "id" }))) {
            Ok(_) | Err(MeiliError::Status { .. }) => {} // Already there is fine; the settings call reports real trouble
            Err(e) => return Err(e),
        }
        let v = self.call("PATCH", &format!("/indexes/{}/settings", self.index), Some(&index_settings(self.max_hits)))?;
        Self::task_uid(&v)
    }

    /// Adds or replaces whole documents. Returns the task uid.
    pub fn put_documents<T: Serialize>(&self, docs: &[T]) -> MeiliResult<u64> {
        let body = serde_json::to_value(docs).map_err(|e| MeiliError::Response(e.to_string()))?;
        Self::task_uid(&self.call("POST", &format!("/indexes/{}/documents?primaryKey=id", self.index), Some(&body))?)
    }

    /// Swaps this index with `other`, so a freshly built index goes live in one step.
    pub fn swap_with(&self, other: &str) -> MeiliResult<u64> {
        Self::task_uid(&self.call("POST", "/swap-indexes", Some(&json!([{ "indexes": [self.index, other] }])))?)
    }

    pub fn delete_index(&self) -> MeiliResult<u64> {
        Self::task_uid(&self.call("DELETE", &format!("/indexes/{}", self.index), None)?)
    }

    /// Waits for an enqueued task to finish and fails if it did.
    pub fn wait(&self, uid: u64, timeout: Duration) -> MeiliResult<()> {
        let start = Instant::now();
        let mut pause = Duration::from_millis(10);
        loop {
            let t = self.call("GET", &format!("/tasks/{uid}"), None)?;
            match t["status"].as_str() {
                Some("succeeded") => return Ok(()),
                Some(s @ ("failed" | "canceled")) => {
                    return Err(MeiliError::Task { uid, status: s.into(), error: t["error"]["message"].to_string() })
                }
                _ if start.elapsed() > timeout => {
                    return Err(MeiliError::Task { uid, status: "still running".into(), error: format!("after {timeout:?}") })
                }
                _ => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(500));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> SearchQuery {
        SearchQuery::from_params(Some("show 1080p".into()), None, None, Some("1_2"), Some("3"),
                                 Some("seeders"), Some("asc"), Some(3), 75, false)
    }

    #[test]
    fn search_body_carries_term_filters_sort_and_page() {
        assert_eq!(search_body(&query()), json!({
            "q": "show 1080p",
            "filter": ["main_category_id = 1", "sub_category_id = 2", "trusted = true AND complete = true",
                       "deleted = false", "hidden = false"],
            "sort": ["seed_count:asc", "id:desc"],
            "page": 3,
            "hitsPerPage": 75,
            "attributesToRetrieve": ["id"],
            "matchingStrategy": "all",
        }));
    }

    #[test]
    fn filter_matches_sql_visibility_rules() {
        let mut q = SearchQuery::new();
        q.user_id = Some(4);
        q.group_id = Some(9);
        q.quality_filter = 1;
        q.hide_anonymous = true;
        assert_eq!(filter(&q), ["uploader_id = 4", "group_id = 9", "remake = false",
                                "deleted = false", "hidden = false", "anonymous = false"]);
        // Moderators: nothing hidden
        let mut q = SearchQuery::new();
        q.include_deleted = true;
        q.include_hidden = true;
        assert!(filter(&q).is_empty());
        // A sub-category only counts with its main category
        let q = SearchQuery::from_params(None, None, None, Some("0_2"), None, None, None, None, 75, true);
        assert!(filter(&q).is_empty());
    }

    #[test]
    fn sorts_newest_first_among_ties() {
        let mut q = SearchQuery::new();
        assert_eq!(sort(&q), ["id:desc"]);
        q.order = SearchOrder::Asc;
        assert_eq!(sort(&q), ["id:asc"]);
        q.sort = SearchSort::Name;
        assert_eq!(sort(&q), ["display_name:asc", "id:desc"]);
        q.sort = SearchSort::Downloads;
        q.order = SearchOrder::Desc;
        assert_eq!(sort(&q), ["download_count:desc", "id:desc"]);
    }

    #[test]
    fn documents_flatten_flags_and_stats() {
        let now = chrono::Utc::now().naive_utc();
        let t = Torrent {
            id: 7,
            flags: (TorrentFlags::BANNED | TorrentFlags::TRUSTED).bits(),
            ..crate::torrent::tests::sample_torrent()
        };
        let s = Statistic { torrent_id: 7, seed_count: 3, leech_count: 4, download_count: 5, last_updated: now };
        let doc = TorrentDoc::new(&t, Some(&s));
        assert!(doc.deleted && doc.trusted && !doc.hidden && !doc.remake);
        assert_eq!((doc.seed_count, doc.leech_count, doc.download_count), (3, 4, 5));
        assert_eq!(TorrentDoc::new(&t, None).seed_count, 0);
    }
}
