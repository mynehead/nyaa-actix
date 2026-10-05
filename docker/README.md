# File storage: local disk or S3

nyaa-actix stores two kinds of files: the info dict of every uploaded torrent (the
`.torrent` download is rebuilt from it) and uploaded avatars. `STORAGE_BACKEND` picks where:

- `local` (default): `TORRENT_STORAGE_PATH` (`./torrents`) and `AVATAR_STORAGE_PATH` (`./avatars`).
- `s3`: one bucket on any S3-compatible service, with the files under `torrents/` and
  `avatars/` (or `<S3_PREFIX>/torrents/` ...). The bucket can stay private; the site
  reads the files and serves them itself.

| Setting | Meaning |
| --- | --- |
| `S3_ENDPOINT` | Service URL; leave unset for AWS. `http://` is allowed for local servers. |
| `S3_BUCKET` | Bucket name (required). Create it in the provider's console first. |
| `S3_REGION` | Region the provider expects; default `us-east-1`. |
| `S3_ACCESS_KEY`, `S3_SECRET_KEY` | Key pair with read and write access to the bucket. |
| `S3_PATH_STYLE` | `true` sends `endpoint/bucket/key` instead of `bucket.endpoint/key`. Needed for self-hosted servers. |
| `S3_PREFIX` | Optional folder inside the bucket, to share it with other data. |

## Hetzner Object Storage

Create a bucket and S3 credentials in the Hetzner Console (Object Storage). The endpoint
and region are the bucket's location: `fsn1` (Falkenstein), `nbg1` (Nuremberg) or `hel1`
(Helsinki).

```env
STORAGE_BACKEND=s3
S3_ENDPOINT=https://fsn1.your-objectstorage.com
S3_REGION=fsn1
S3_BUCKET=my-nyaa-files
S3_ACCESS_KEY=...
S3_SECRET_KEY=...
S3_PATH_STYLE=false
```

## Backblaze B2

Create a private bucket, then an **application key** limited to that bucket with read and
write access (the master key does not work with B2's S3 API). The bucket page shows the
endpoint, for example `s3.eu-central-003.backblazeb2.com`; the region is the part in the
middle. `keyID` is the access key, `applicationKey` the secret.

```env
STORAGE_BACKEND=s3
S3_ENDPOINT=https://s3.eu-central-003.backblazeb2.com
S3_REGION=eu-central-003
S3_BUCKET=my-nyaa-files
S3_ACCESS_KEY=<keyID>
S3_SECRET_KEY=<applicationKey>
S3_PATH_STYLE=false
```

## Self-hosted, written in Rust

Two S3 servers written in Rust work with nyaa-actix; both are tested here (CI runs the
storage tests against Garage on every push).

- **[Garage](https://garagehq.deuxfleurs.fr/)** (AGPL-3.0, image `dxflrs/garage:v2.4.1`).
  Built for small, self-hosted clusters, including nodes in different places on modest
  hardware; replication is built in. Mature (its authors have run it in production since 2020), with a small API
  surface that covers everything nyaa-actix needs. The recommended choice for running
  your own storage. AGPL only matters if you modify Garage itself.
- **[RustFS](https://rustfs.com/)** (Apache-2.0, image `rustfs/rustfs:1.0.1`). A MinIO-style
  server with a web console; 1.0 went GA in September 2026. Easier first start (no layout
  step) and a fuller S3 feature set, but much younger than Garage.

### Local test servers

`docker/compose.yml` runs either one for development:

```sh
# Garage: S3 API on http://localhost:3900, bucket `nyaa`
docker compose -f docker/compose.yml up -d garage
docker/garage/init.sh
```

The `S3_*` values in `.env.example` match this server; set `STORAGE_BACKEND=s3` to use it.
On Windows run `init.sh` from Git Bash or WSL.

```sh
# RustFS: S3 API on http://localhost:9000, console on http://localhost:9001
docker compose -f docker/compose.yml --profile rustfs up -d rustfs
# create the bucket (curl 7.75 or newer signs the request itself)
curl --aws-sigv4 "aws:amz:us-east-1:s3" --user nyaa-dev:nyaa-dev-secret -X PUT http://localhost:9000/nyaa
```

For RustFS use `S3_ENDPOINT=http://localhost:9000`, `S3_REGION=us-east-1`,
`S3_ACCESS_KEY=nyaa-dev`, `S3_SECRET_KEY=nyaa-dev-secret`, `S3_PATH_STYLE=true`.

The credentials in these files are public development values. Never expose these servers
to the internet as configured.

### Running the S3 tests locally

The storage tests also run against a real server when `NYAA_TEST_S3_BUCKET` is set, with
the other settings given the same way (`NYAA_TEST_S3_ENDPOINT`, ...). For the Garage server
above, copy the variables from the `s3` job in `.github/workflows/ci.yml` and run
`cargo test "storage::"`.

## Moving existing files to S3

```sh
nyaa-actix migrate-storage --dry-run   # list what would be copied
nyaa-actix migrate-storage             # copy
```

It copies everything in the two local folders to the bucket the `S3_*` settings name,
under the same keys, and skips files the bucket already has with the same size, so it can
be stopped and run again. Local files are not deleted. To switch without losing uploads:
run it, stop the server, run it once more for uploads made in the meantime, then set
`STORAGE_BACKEND=s3` and start the server.
