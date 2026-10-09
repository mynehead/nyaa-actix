# Roadmap: nyaa-actix as a nyaadevs/nyaa clone on torrust-actix

Goal: a Rust/Actix clone of [nyaadevs/nyaa](https://github.com/nyaadevs/nyaa) that uses
[torrust-actix](https://github.com/Power2All/torrust-actix) (v4.2.x) for announce and scrape.

The detailed announce/scrape design (stats sync job, `TRACKER_ANNOUNCE_URLS`) lives in the
"Tracker design: announce and scrape" doc from the tracker thread. This file is the overall plan
and the feature-gap list against upstream nyaa.

## 1. How torrust-actix fits

### Decision: run it as a separate service, not embedded

torrust-actix does ship a `lib.rs`, but embedding it is a poor fit:

- It is edition 2024 with a large dependency tree (Sentry, Redis/Memcache, io_uring UDP,
  RtcTorrent, cluster mode, a fontconfig system dependency) that the site does not need.
- It starts its own Actix HTTP servers, UDP listeners and background cleanup threads. Running
  those inside the site process couples restarts, scaling and crashes of two very different
  workloads (a high-QPS UDP tracker vs. a template-rendering website).
- Upstream nyaa also runs its tracker as a separate process and talks to it through a small API.

So: run the official binary or Docker image (`power2all/torrust-actix`) next to the site, e.g. in a
`docker-compose.yml`. Its defaults are UDP and HTTP announce on `:6969` and the API on `:8080`,
which collides with the site's hardcoded `0.0.0.0:8080`, so move one of them (and keep the API
port internal).

### The three touch points

| Concern | Upstream nyaa | nyaa-actix with torrust-actix |
|---|---|---|
| Which hashes may announce | Writes `insert`/`remove` rows to a `trackerapi` queue table, the tracker consumes it | Call torrust-actix `POST/DELETE /api/whitelist/{info_hash}` (token in `Authorization` header) with `TRACKER__WHITELIST_ENABLED=true` |
| Seed/leech/complete counts | Tracker writes into `statistics` | Background job scrapes torrust-actix (BEP 48 multi-hash scrape) and upserts `nyaa_statistics` |
| Tracker URLs in magnets / .torrent | `MAIN_ANNOUNCE_URL` + `trackers` table (`trackers.txt` defaults) | `TRACKER_ANNOUNCE_URLS` config, main announce first; later a `trackers` table |

Built (`src/tracker.rs`): instead of an outbox table, handlers queue the ids of changed
torrents for a background thread, which sends them in batches. Whenever the tracker's
start time changes (or a call failed) the thread sends the whole whitelist again, so an
upload never fails because the tracker is briefly down, and deletes/bans/user nukes that
happened meanwhile still reach it. Counts come from `GET /api/torrents` in batches of 200.

Events that must enqueue: upload (`insert`), torrent delete or ban (`remove`), undelete/unban
(`insert`), user nuke (`remove` for each torrent).

### Reading stats: two stages

1. **Now (SQLite, separate DBs):** scrape job every few minutes, batches of ~50 hashes per request
   to stay under URL limits, upserts `nyaa_statistics`. Tracker-agnostic and needs no shared DB.
2. **At scale (shared PostgreSQL):** torrust-actix lets you rename its persisted torrents table and
   columns (`DATABASE_STRUCTURE__TORRENTS__TABLE_NAME`, `COLUMN_INFOHASH`, `COLUMN_SEEDS`,
   `COLUMN_PEERS`, `COLUMN_COMPLETED`, `BIN_TYPE_INFOHASH=true`). Point it at a `tracker_torrents`
   table in the site's database and have the listing join on `info_hash`, dropping the scrape job.
   Its `database.chunk_size` setting exists exactly to avoid lock contention with a website
   sharing the DB. Sharing one SQLite file between both processes is not recommended.

The torrust-actix JSON API (`GET /api/torrents`) returns full peer lists per hash, so it is too
heavy to use for bulk stats; use scrape or the shared table.

### Site config to add

```
TRACKER_ANNOUNCE_URLS=udp://tracker.example:6969/announce,http://tracker.example:6969/announce
TRACKER_API_URL=http://torrust:8080
TRACKER_API_TOKEN=...            # matches TRACKER__API_KEY on the tracker
TRACKER_WHITELIST=true            # false = open tracker, skip whitelist calls
ENFORCE_MAIN_ANNOUNCE_URL=false   # upstream option: reject uploads missing our announce
STATS_SYNC_INTERVAL_SECS=300
```

Code that changes: `Torrent::magnet_uri` and `rebuild_torrent` callers currently pass `&[]` for
trackers (`src/handlers/torrents.rs`), and `rebuild_torrent` hardcodes a `https://nyaa.si/view/`
comment that should come from a `SITE_URL` setting.

## 2. Feature gaps vs. nyaadevs/nyaa

Status of this repo as of the initial port. "Stub" = route exists but returns placeholder text.

### Core browsing
| Feature | Upstream | Here |
|---|---|---|
| Home/search with category, filter, sort | yes | yes (SQL `LIKE`) |
| Full-text search (Elasticsearch, MySQL fulltext) | yes | missing; SQLite FTS5 or Postgres `tsvector` is the natural replacement |
| RSS feed (`/?page=rss`, `nyaa:` xmlns with seeders/leechers/infoHash) | yes | yes (`/?page=rss`, `/rss`, `&magnets`; see `docs/api.md`) |
| Torrent file list on view page | yes (`torrents_filelist`) | missing; parse `info.files` at upload |
| Seeders/leechers/downloads columns + sort | yes | yes, pulled from torrust-actix (`TRACKER_API_URL`, `src/tracker.rs`) |
| Sukebei flavor (second category set, table prefix) | yes | `SITE_FLAVOR` exists, no second schema |
| `/rules`, `/help`, `/xmlns/nyaa`, `/trusted` info pages | yes | yes |

### Torrents
| Feature | Upstream | Here |
|---|---|---|
| Upload (web) | yes | yes |
| Upload API (`/api/upload`, `/api/v2/upload`) and info API (`/api/info/<id or hash>`) | yes | yes, login required (see `docs/api.md`) |
| Edit torrent (`/view/<id>/edit`), delete/undelete, ban | yes | missing |
| Download rebuilds .torrent with announce list | yes | yes, but empty tracker list |
| Magnet link | yes | yes, but no `tr=` params |
| Upload rate limits, min anonymous size, raid mode | yes | missing |
| Duplicate info_hash detection on upload | yes | yes |

### Community
| Feature | Upstream | Here |
|---|---|---|
| Comments: post, edit, delete, lock | yes | table exists, no handlers |
| Reports + moderator queue | yes | stub (`/admin/reports`) |
| Admin/mod action log | yes | stub (`/admin/log`), no table |
| User and IP bans, range bans | yes | stub (`/admin/bans`), `bans` table only |
| User nuke (torrents/comments) | yes | missing |
| Trusted applications + review | yes | missing |
| User comments page | yes | missing |
| Groups | not upstream | present (extra feature, keep) |

### Accounts
| Feature | Upstream | Here |
|---|---|---|
| Register/login/logout/profile | yes | yes |
| Email verification, password reset | yes | missing (needs mail backend: SMTP/Mailgun) |
| reCAPTCHA / account age gates | yes | missing |
| Email domain blacklists, per-IP account cooldown | yes | missing |
| Gravatar | yes | config flag only |
| CSRF protection | yes (Flask-WTF) | missing; required before any POST form goes live |

## 3. Order of work

1. **Build and boot.** Compile fixes (separate thread "Fix the build so the site compiles") plus
   the startup crash: Tera templates call Rust methods (`torrent.is_trusted()`,
   `current_user.level_str()`, `torrent.row_class()`, ...) which Tera cannot do, so the server
   panics loading templates even once it compiles.
2. **Tracker integration.** docker-compose with torrust-actix, tracker config, announce URLs in
   magnets/.torrent, whitelist outbox, stats scrape job. Seed/leech columns become real.
3. **Safety before exposure.** CSRF tokens, upload rate limits, bans enforced on login/upload.
4. **Moderation.** Torrent edit/delete/ban, comments, reports, admin log, user nuke.
5. **Discovery.** File lists, RSS with nyaa xmlns, info/upload APIs, full-text search.
6. **Accounts extras.** Email verification, password reset, captcha, trusted applications.
7. **Sukebei flavor**, if wanted.
