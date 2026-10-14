# RSS feed and JSON API

Both follow upstream nyaa's URLs and formats, so existing feed readers and upload scripts
work unchanged. Nothing here needs a setting; absolute URLs use `SITE_URL`.

## RSS

`/?page=rss` (or `/rss`) is the listing as an RSS 2.0 feed. It takes the listing's
parameters, including upstream's older aliases:

| Parameter | Aliases | Meaning |
|---|---|---|
| `q` | `term` | search term, with the search operators |
| `c` | `cats` | category, `main_sub` (`1_2`) |
| `f` | `filter` | 0 all, 1 no remakes, 2 trusted only |
| `u` | `user` | uploader's username (404 if unknown) |
| `s`, `o` | | sort and order |
| `p` | `page`, `offset` | page |
| `magnets` or `m` | | link items to magnet URIs instead of `.torrent` files |

Items carry `nyaa:seeders`, `nyaa:leechers`, `nyaa:downloads`, `nyaa:infoHash`,
`nyaa:categoryId`, `nyaa:category`, `nyaa:size`, `nyaa:comments`, `nyaa:trusted` and
`nyaa:remake`, described at `/xmlns/nyaa`. The navbar's RSS link gives the feed for the
current search.

## JSON API

Every call signs in with HTTP Basic auth: username (or email) and password, or in place
of the password an API key made under Profile > API keys (`curl -u name:nyaa_...`).
Accounts with two-factor sign-in, or whose class must have it (`MFA_REQUIRED_LEVEL`), can
only use keys. A key is shown once, can be revoked there, and works until it is. Limited accounts may not
upload or use keys. Failed
passwords count towards the login form's limits. Errors come back as
`{"errors": [...]}` (auth and lookups) or `{"errors": {"field": ["message"]}}` (uploads).

| Status | Body |
|---|---|
| 403 | `["Bad authorization"]` (no or malformed credentials) |
| 403 | `["Incorrect username or password"]` |
| 403 | `["Accounts with two-factor sign-in need an API key. ..."]` (password sent for such an account) |
| 429 | too many failed attempts, try again in 15 minutes |

### `GET /api/info/<id or hex info hash>`

```
curl -u name:password https://nyaa.example/api/info/1234
```

Returns `submitter` (null for anonymous uploads, except to the uploader and moderators),
`url`, `id`, `name`, `creation_date`, `hash_b32`, `hash_hex`, `magnet`, `main_category`,
`main_category_id`, `sub_category`, `sub_category_id`, `information`, `description`,
`stats` (`seeders`, `leechers`, `downloads`), `filesize`, `files` (folders are objects,
files their size), `is_trusted`, `is_complete`, `is_remake`. Unknown, deleted or banned
torrents (the last two except for moderators) answer 400
`["Query was not a valid id or hash."]`.

### `POST /api/upload` (also `/api/v2/upload`)

Multipart form with the `.torrent` file as `torrent` and the details as JSON in
`torrent_data`:

```
curl -u name:password -F torrent=@show.torrent \
  -F 'torrent_data={"category": "1_2", "name": "Show - 01", "information": "", "description": "",
                    "anonymous": false, "hidden": false, "complete": false, "remake": false, "trusted": true}' \
  https://nyaa.example/api/upload
```

Only `category` is required; a blank `name` uses the torrent's own. `trusted` defaults to
true and only applies to users who may mark uploads trusted. The checks are the upload
page's. Success returns `url`, `id`, `name`, `hash` and `magnet`; failure is 400 with
errors under `torrent`, `name`, `category`, `information` or `description`.

Unlike upstream, uploads always need an account (no anonymous, account-less uploads).

## Importing your torrents from another nyaa site

`tools/import_nyaa.py` copies your own uploads from nyaa.si, sukebei or any site running
upstream's code (or this one) to this site. It needs only Python 3.8 or newer, no packages,
and no password for the source:

```
python tools/import_nyaa.py --source https://nyaa.si --source-user MyName \
                            --target https://nyaa.example --key MyName:nyaa_... --dry-run
```

Without `--cookie` it lists your public uploads from the source's RSS feed
(`?page=rss&u=MyName`). Anonymous and hidden uploads are never in that feed; to include
them, sign in to the source in your browser and pass its cookie: `--cookie "session=..."`
(the whole `Cookie` header from the browser's developer tools also works, or set
`NYAA_SOURCE_COOKIE`). The script then lists your own user page, which shows you all of
your uploads, and anonymous or hidden ones stay so here.

For each torrent, oldest first, it reads the details from the torrent's page, skips ones
already here or uploaded by another account, and uploads the rest with the same name,
category, information, description and anonymous/hidden/remake/trusted flags. The info
hash stays the same, so the swarm (and its seeders) is shared with the source. Drop
`--dry-run` to import; running it again only picks up what is new. `--limit 5` stops after
five torrents that are not here yet, to try it out or import in batches. `--delay` sets the
seconds between requests to the source (default 2). The key can also come from
`NYAA_API_KEY`.

- If this site requires its tracker in uploads (`ENFORCE_MAIN_ANNOUNCE_URL`), the script
  adds it to the file's tracker list; the info hash does not change.
- Accounts under the upload rate limit get told how long to wait; the script waits and
  carries on.
- Imports get today's date and show up as new uploads. The "complete" flag is not shown on
  torrent pages, so it is not copied.
- nyaa.si's `/api/info` answers 502, so the script reads the pages instead; if nyaa.si
  changes their layout, it says "unexpected page layout".
- A site that limits how far its listings page (nyaa.si does; `MAX_PAGES` here) leaves out
  the oldest uploads of very large accounts.
