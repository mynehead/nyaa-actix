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

   For a public site also read [Going public](#going-public) before the first start.
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

## Going public

### Settings

On top of the ones above, a public site needs these in `.env`:

| Setting | Why |
| --- | --- |
| `SECRET_KEY` | A new one from `openssl rand -hex 64`. The placeholder in `.env.example` is long enough to pass the start-up check, so the site does not refuse it. Keep a copy somewhere safe: a lost key makes every two-factor setup unreadable. |
| `SITE_URL=https://nyaa.example` | Exactly the address visitors use. Behind a proxy the site checks form posts against it, so a wrong value turns every post away with a 403. |
| `REGISTRATION_MODE=invite` | The default is `open`, where anyone can sign up. With `invite`, accounts need a code from Admin > Invites. |
| `NYAA_PORT=127.0.0.1:8080` | Publishes the site on localhost only, for the proxy. Docker writes its own firewall rules, so a plain `8080` is reachable from outside without TLS even with ufw or firewalld blocking it. |
| `TRUSTED_PROXIES=172.16.0.0/12` | Requests from a proxy on the host arrive from the Docker network's gateway. Without this every visitor has that one address, so bans and rate limits hit everybody. `docker network inspect <folder>_default` (the folder holding compose.yml) shows the gateway; Docker's networks are in 172.16.0.0/12 unless you changed its address pools. |
| `MFA_REQUIRED_LEVEL=moderator` | Moderators and admins must set up two-factor before they can use the site. |
| `MAIL_BACKEND=smtp`, `MAIL_FROM_ADDRESS`, `SMTP_*` | Without mail there is no password reset. Set up SPF, DKIM and DMARC for the sender's domain, or the mails land in spam. |

After the first start, create the admin, turn on two-factor for it, make the first codes under
Admin > Invites, and check that `/register` without a code is turned away.

### TLS with Caddy

Caddy (2.8 or later) gets and renews Let's Encrypt certificates on its own for every domain named in its
config; it only needs ports 80 and 443 open and the domain's DNS pointing at the server. It
also sets `X-Forwarded-For` and has no upload size limit, so the config below is complete,
including the [maintenance page](#a-maintenance-page-while-the-site-is-down) for when the site
is stopped. Copy `static/maintenance.html` to `/srv/nyaa/maintenance.html` first.

```caddyfile
nyaa.example {
	reverse_proxy 127.0.0.1:8080
	handle_errors 502 503 504 {
		root * /srv/nyaa
		rewrite * /maintenance.html
		header Retry-After 300
		header Cache-Control no-store
		file_server {
			status 503
		}
	}
}
```

Traefik can fetch certificates automatically too, through labels on the `site` service, but
the maintenance page then needs an errors middleware and a second service to serve the file.
Caddy needs neither. With nginx, certificates come from certbot, and the config is under
[Maintenance](#a-maintenance-page-while-the-site-is-down).

Open only 80 and 443 (TCP) for the proxy and 6969 (TCP and UDP) for the tracker.

### Tracker

Once the site is up, upload a torrent and open it in a client from another network, over UDP
and HTTP. After `TRACKER_STATS_SYNC_SECS` (default 300) the torrent page shows its seeders. Also
check that the tracker sees the peers' real addresses, IPv6 ones included (a second client from
another network should find the first). If Docker's IPv6 is off, IPv6 connections pass through
its userland proxy and all arrive from one address. Then turn on IPv6 for the compose network
(`networks: default: enable_ipv6: true` in compose.yml, with a `subnet` if your Docker version
asks for one) and recreate the stack.

### Backups

The database and the `site-data` volume are both needed: the torrent files live only in the
volume (unless `STORAGE_BACKEND=s3`; then turn on versioning or back up the bucket). A daily job,
for example `/etc/cron.daily/nyaa-backup` (made executable, with the paths adjusted):

```sh
#!/bin/sh
set -e
cd /srv/nyaa-actix                 # where compose.yml and .env are
dir=/srv/backup/nyaa
mkdir -p "$dir/site-data"
docker compose exec -T postgres pg_dump -U nyaa -Fc nyaa > "$dir/nyaa-$(date +%F).dump"
docker compose cp site:/data/. "$dir/site-data/"
find "$dir" -name 'nyaa-*.dump' -mtime +14 -delete
```

A backup on the same disk does not survive the server, so copy `$dir` (and `.env`) to another
machine or provider, for example with restic, borg or rclone. Try a restore once before you
need one:

```sh
docker compose exec -T postgres createdb -U nyaa restore_test
docker compose exec -T postgres pg_restore -U nyaa -d restore_test < /srv/backup/nyaa/nyaa-2026-01-01.dump
docker compose exec -T postgres dropdb -U nyaa restore_test
```

### Logs and monitoring

Docker keeps container logs without a size limit, and the site logs one line per request. Cap
them in `/etc/docker/daemon.json`, then `systemctl restart docker` and
`docker compose up -d --force-recreate` (the limit applies to new containers only):

```json
{ "log-driver": "json-file", "log-opts": { "max-size": "50m", "max-file": "5" } }
```

`restart: unless-stopped` restarts a crashed container but tells nobody. Have an outside uptime
check (Uptime Kuma, healthchecks.io or your host's monitoring) call `https://nyaa.example/rules`
and the tracker's HTTP port, and alert on disk space.

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

Migrations run on start, so the database follows the code. Back up first, for example by
running the [backup job](#backups) by hand.

## Maintenance

There are three levels, from least to most disruptive:

| What you need | How | Visitors see |
| --- | --- | --- |
| Stop changes for a while (big import, moderation clean-up) | `MAINTENANCE_MODE=true` | the site as usual with a notice; posts, uploads and registration are turned back |
| Close the site but keep it running (check a deploy before opening) | also `MAINTENANCE_MODE_OFFLINE=true` | `static/maintenance.html` as a 503 with `Retry-After`; moderators and admins log in at `/login` and see the read-only site |
| The site container is stopped or restarting (update, database work) | the reverse proxy below | the same page, served by the proxy |

Settings are read on start, so after changing them in `.env` run `docker compose up -d site`;
the proxy covers the seconds the site needs to restart. `MAINTENANCE_MODE_MESSAGE` replaces
the page's text, and `MAINTENANCE_MODE_RETRY_AFTER` sets the seconds in `Retry-After`
(default 300), which tells crawlers and RSS readers to come back later rather than drop pages.

The tracker is a separate service on port 6969 and is not affected by any of these:
clients keep announcing while the site is offline or stopped. Do not route announces through
the maintenance fallback below, since a torrent client cannot read an HTML page. Only
restarting the tracker itself matters: it then refuses announces until the site has sent it
the whitelist again, so restart the tracker while the site is up.

### A maintenance page while the site is down

Without a proxy, a stopped site means a browser error. Put the proxy in front of port 8080
and have it serve `static/maintenance.html` whenever it cannot reach the site. The page is
self-contained (no other files needed), and the site's own 503s pass through unchanged.
Copy `static/maintenance.html` to the proxy host, here `/srv/nyaa/maintenance.html`.

Caddy (2.8 or later): the config under [TLS with Caddy](#tls-with-caddy) already does this.

nginx:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    # The visitor's address, for TRUSTED_PROXIES
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    # Torrent files up to 10 MiB; nginx allows 1 MiB by default
    client_max_body_size 11m;
    # Only nginx's own errors (site unreachable); the site's 503 page passes through
    error_page 502 504 =503 /maintenance.html;
}
location = /maintenance.html {
    internal;
    root /srv/nyaa;
    add_header Retry-After 300 always;
    add_header Cache-Control no-store always;
}
```

With the proxy in place, a full maintenance run is:

```sh
docker compose stop site          # visitors get the maintenance page from the proxy
docker compose exec postgres ...  # database work, backups, and so on
docker compose up -d --build site # migrations run on start, then the site answers again
```

## Notes

- The site container runs as an unprivileged user (uid 10001) and writes only to `/data`.
- Only 8080 and 6969 are published; PostgreSQL, Meilisearch and the tracker API stay on the
  compose network.
- The tracker keeps peers and its whitelist in memory, so a restart empties the swarm until
  clients announce again (within one announce interval). The site reaches its API at
  `http://tracker:8080` with `TRACKER_API_KEY`, sends the whole whitelist again when it sees
  the tracker restarted, and pulls seeders, leechers and completed counts every
  `TRACKER_STATS_SYNC_SECS` (default 300). The tracker only accepts announces for torrents
  on that whitelist, so until the site has reached it once, announces are refused.
- The image runs without compose too, given a PostgreSQL server:
  `docker run -p 8080:8080 -v nyaa-data:/data -e SECRET_KEY=... -e DATABASE_URL=postgres://... nyaa-actix`.
- On Windows use Docker Desktop with the WSL 2 backend; the commands above work the same in
  PowerShell. The first build compiles every dependency and takes a while; later builds
  reuse BuildKit's cache.
