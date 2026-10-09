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

Every call signs in with HTTP Basic auth: username (or email) and password. Failed
passwords count towards the login form's limits. Errors come back as
`{"errors": [...]}` (auth and lookups) or `{"errors": {"field": ["message"]}}` (uploads).

| Status | Body |
|---|---|
| 403 | `["Bad authorization"]` (no or malformed credentials) |
| 403 | `["Incorrect username or password"]` |
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
