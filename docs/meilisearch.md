# Search with Meilisearch

Meilisearch is optional. Without it, search runs on SQLite as before. With it, text
searches and the seeders, leechers and downloads sorts go to Meilisearch, which answers
them without scanning every torrent. Plain listings (no search term, sorted by date, name
or size) stay on SQLite, where they are already fast. This is the role Elasticsearch
plays in upstream nyaa.

SQLite stays the source of truth. Meilisearch only decides which torrent ids match and in
what order; the rows are then loaded from SQLite. If Meilisearch is down, slow (over 2 s)
or misconfigured, the site logs a warning and searches SQLite instead.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `MEILI_URL` | unset | Meilisearch address, such as `http://127.0.0.1:7700`. Unset or empty turns Meilisearch off. |
| `MEILI_KEY` | unset | Master key or an API key that may search, add documents and manage indexes. |
| `MEILI_INDEX` | `torrents` | Index name. |
| `MEILI_MAX_HITS` | `10000` | Most results a search counts and pages through. |
| `MEILI_STATS_SYNC_SECS` | `60` | How often changed tracker stats are pushed to the index. |

## How the index stays current

- Searches only use the index once the server has checked it. At start and then every
  `MEILI_STATS_SYNC_SECS`, the server compares the number of torrents in the index with
  the database. When the index is missing, holds a different number of torrents, or an
  update failed to reach it, the server rebuilds it in the background and searches
  SQLite until that is done. So setting `MEILI_URL` on an existing database, or
  Meilisearch being down during an upload, needs no manual step.
- An upload, an edit, and a delete, ban or undelete on the edit page push that torrent
  to the index right away.
- A background task pushes the torrents whose seeder, leecher and download counts
  changed (by `nyaa_statistics.last_updated`) since its last run. On server start it
  pushes all of them once.
- `nyaa-actix reindex` does the same rebuild by hand: it builds a complete new index from
  the database under a temporary name and then swaps it in, so searches keep working
  while it runs. 100,000 torrents take about 13 seconds. Edits made while it runs can be
  missed, so run it when the site is quiet.

## Running it on Windows

1. Download `meilisearch-windows-amd64.exe` from
   <https://github.com/meilisearch/meilisearch/releases/latest> into a folder of its own
   (it keeps its data in `data.ms` next to where you start it).
2. Start it with a master key of at least 16 bytes:

   ```powershell
   .\meilisearch-windows-amd64.exe --master-key "a-long-local-dev-key-123"
   ```

   With Docker Desktop instead:

   ```powershell
   docker run -d --name meili -p 7700:7700 -e MEILI_MASTER_KEY=a-long-local-dev-key-123 -v meili_data:/meili_data getmeili/meilisearch:v1.54
   ```

3. Add to `.env`:

   ```
   MEILI_URL=http://127.0.0.1:7700
   MEILI_KEY=a-long-local-dev-key-123
   ```

4. Start the site as usual (`cargo run`). It builds the index on start; the log says
   "Rebuilding Meilisearch index" and then "Indexed N torrents". To rebuild by hand, run
   `cargo run -- reindex`, or `.\target\debug\nyaa-actix.exe reindex` while the server
   is running, because Windows won't let `cargo run` replace an exe that is in use.

Use `127.0.0.1` rather than `localhost` in `MEILI_URL`; Meilisearch listens on IPv4 by
default, and Windows tries IPv6 first for `localhost`.

## Linux and production

Install Meilisearch from the release page or the `getmeili/meilisearch` Docker image, run it
with `--env production` and a master key, and give the site an API key limited to the
`torrents` index rather than the master key. Keep Meilisearch on a private address; the
site is its only client.

## Tests and CI

`cargo test` runs the Meilisearch round-trip test only when `MEILI_TEST_URL` is set, and
skips it otherwise:

```sh
docker run -d -p 7700:7700 -e MEILI_MASTER_KEY=testmasterkey1234567890 getmeili/meilisearch:v1.54
MEILI_TEST_URL=http://127.0.0.1:7700 MEILI_TEST_KEY=testmasterkey1234567890 cargo test
```

(On Windows PowerShell: `$env:MEILI_TEST_URL="http://127.0.0.1:7700"; $env:MEILI_TEST_KEY="..."; cargo test`.)

It builds a throwaway index from a small database, checks that Meilisearch returns the
same torrents in the same order as SQLite for searches, category and quality filters,
sorts and paging, and that edits and stats changes reach the index, then deletes the
index. CI runs Meilisearch as a service container, so the test always runs there.

## Search syntax

Every word must match (as upstream's Elasticsearch `AND`), and the last word also
matches as a prefix. `"quoted phrases"` match exactly and `-word` excludes a word. Typo
tolerance is off, since release names that differ by a letter are usually different
releases. Unlike the SQLite search, a word inside another word does not match: `dragon`
finds "Dragon Ball" but `ragon` does not.
