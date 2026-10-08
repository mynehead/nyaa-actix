# Running nyaa-actix with Docker

`compose.yml` in the repository root runs the whole stack on one host:

| Service | Image | Purpose |
| --- | --- | --- |
| `site` | built from `Dockerfile` | the web site, port 8080 |
| `postgres` | `postgres:17` | the site database |
| `tracker` | `power2all/torrust-actix:v4.2.23` | announce and scrape on port 6969 (TCP and UDP) |
| `meilisearch` | `getmeili/meilisearch:v1.54` | optional search, only with `--profile search` |

File storage stays on the `site-data` volume unless `.env` sets `STORAGE_BACKEND=s3`; see
[README.md](README.md) for S3 providers and the Garage and RustFS dev servers.

## First start

1. Copy `.env.example` to `.env` and set at least:
   - `SECRET_KEY` (`openssl rand -hex 64`)
   - `POSTGRES_PASSWORD` and `TRACKER_API_KEY` (any long random strings)
   - `SITE_URL` and `TRACKER_ANNOUNCE_URLS` to the addresses people will use, for example
     `https://nyaa.example` and `udp://tracker.nyaa.example:6969/announce,http://tracker.nyaa.example:6969/announce`.
     These end up in every magnet and `.torrent` file, so `localhost` only works on your own machine.
2. Build and start:
   ```sh
   docker compose up -d --build
   ```
   The site runs the database migrations itself on start. `docker compose ps` shows each
   service as `healthy` once it answers.
3. Create the first admin:
   ```sh
   docker compose exec site nyaa-actix create-user admin <password> --level admin
   ```
4. Open http://localhost:8080 (or the port in `NYAA_PORT`).

The other subcommands work the same way: `docker compose exec site nyaa-actix migrate-storage`,
`docker compose exec site nyaa-actix reindex`.

## Search

```sh
docker compose --profile search up -d
```

and in `.env`:

```env
MEILI_MASTER_KEY=<at least 16 random bytes>
MEILI_URL=http://meilisearch:7700
MEILI_KEY=<same as MEILI_MASTER_KEY>
```

Restart the site after changing `.env` (`docker compose up -d site`). It builds the index on
first start. Leave `MEILI_URL` empty to search with PostgreSQL only.

## Updating

```sh
git pull
docker compose up -d --build
```

Migrations run on start, so the database follows the code. Back up first:

```sh
docker compose exec postgres pg_dump -U nyaa nyaa > nyaa-$(date +%F).sql
```

The `site-data` volume holds uploaded torrents and avatars when storage is local; back it
up too (or use S3).

## Notes

- The site container runs as an unprivileged user (uid 10001) and writes only to `/data`.
- Put a reverse proxy with TLS (Caddy, nginx, Traefik) in front of port 8080 for a public
  site, and keep `SITE_URL` on the `https://` address. Only 8080 and 6969 are published;
  PostgreSQL, Meilisearch and the tracker API stay on the compose network.
- The tracker keeps peers and its whitelist in memory, so a restart empties the swarm until
  clients announce again (within one announce interval). The site reaches its API at
  `http://tracker:8080` with `TRACKER_API_KEY`, sends the whole whitelist again when it sees
  the tracker restarted, and pulls seeders, leechers and completed counts every
  `TRACKER_STATS_SYNC_SECS` (default 300). The tracker only accepts announces for torrents
  on that whitelist, so until the site has reached it once, announces are refused.
- The image runs without compose too, with SQLite on the volume:
  `docker run -p 8080:8080 -v nyaa-data:/data -e SECRET_KEY=... nyaa-actix`.
- On Windows use Docker Desktop with the WSL 2 backend; the commands above work the same in
  PowerShell. The first build compiles every dependency and takes a while; later builds
  reuse BuildKit's cache.
