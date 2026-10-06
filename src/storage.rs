//! Where uploaded files live: stored torrent info dicts and avatars.
//!
//! `STORAGE_BACKEND=local` (the default) keeps them on disk in TORRENT_STORAGE_PATH and
//! AVATAR_STORAGE_PATH, laid out as before. `STORAGE_BACKEND=s3` puts them in a bucket on
//! any S3-compatible service (Hetzner Object Storage, Backblaze B2, Garage, RustFS, MinIO,
//! AWS), under `torrents/` and `avatars/`:
//!
//! - `S3_BUCKET` (required), `S3_ACCESS_KEY`, `S3_SECRET_KEY`
//! - `S3_ENDPOINT`, e.g. `https://fsn1.your-objectstorage.com`; leave unset for AWS
//! - `S3_REGION` (default `us-east-1`); Garage wants `garage`, Hetzner and B2 their location
//! - `S3_PATH_STYLE=true` for self-hosted servers reached by IP or a single host name
//! - `S3_PREFIX` puts both folders under a prefix, to share a bucket with other data
//!
//! Files are always served through the site, so the bucket can stay private.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::prefix::PrefixStore;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::config::Config;

/// The kinds of stored files, each in its own folder (local) or prefix (S3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The bencoded info dict of an uploaded torrent; .torrent files are rebuilt from it.
    TorrentInfo,
    /// A 256x256 PNG; see `utils::avatar`.
    Avatar,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::TorrentInfo, Kind::Avatar];

    /// The S3 prefix for this kind.
    fn folder(self) -> &'static str {
        match self {
            Kind::TorrentInfo => "torrents",
            Kind::Avatar => "avatars",
        }
    }

    /// The key within the folder: the same relative path the local layout always used,
    /// so files copy across backends unchanged.
    pub fn key(self, id: i32) -> Path {
        match self {
            Kind::TorrentInfo => Path::from(format!("{}/{}.torrent.info", id / 1000, id)),
            Kind::Avatar => Path::from(format!("{}.png", id)),
        }
    }
}

/// S3 settings, read from the environment.
#[derive(Clone, Debug, PartialEq)]
pub struct S3Settings {
    pub endpoint: Option<String>,
    pub bucket: String,
    pub region: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub path_style: bool,
    pub prefix: String,
}

impl S3Settings {
    /// Reads the S3_* variables through `var`, so tests don't touch the process environment.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |key: &str| var(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Ok(S3Settings {
            endpoint: var("S3_ENDPOINT").map(|e| e.trim_end_matches('/').to_string()),
            bucket: var("S3_BUCKET").ok_or("STORAGE_BACKEND=s3 needs S3_BUCKET")?,
            region: var("S3_REGION").unwrap_or_else(|| "us-east-1".into()),
            access_key: var("S3_ACCESS_KEY"),
            secret_key: var("S3_SECRET_KEY"),
            path_style: match var("S3_PATH_STYLE").as_deref() {
                None => false,
                Some(v) => v.parse().map_err(|_| format!("S3_PATH_STYLE must be true or false, not `{v}`"))?,
            },
            prefix: var("S3_PREFIX").map(|p| p.trim_matches('/').to_string()).unwrap_or_default(),
        })
    }

    fn build(&self) -> Result<Arc<dyn ObjectStore>, String> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&self.bucket)
            .with_region(&self.region)
            .with_virtual_hosted_style_request(!self.path_style);
        if let Some(endpoint) = &self.endpoint {
            builder = builder.with_endpoint(endpoint).with_allow_http(endpoint.starts_with("http://"));
        }
        if let (Some(key), Some(secret)) = (&self.access_key, &self.secret_key) {
            builder = builder.with_access_key_id(key).with_secret_access_key(secret);
        }
        let store = builder.build().map_err(|e| format!("S3 storage: {e}"))?;
        Ok(Arc::new(store))
    }
}

/// The configured file store, shared by all workers.
#[derive(Clone, Debug)]
pub struct Storage {
    torrents: Arc<dyn ObjectStore>,
    avatars: Arc<dyn ObjectStore>,
    description: String,
}

impl Storage {
    /// The backend STORAGE_BACKEND names, `local` when unset.
    pub fn from_env(cfg: &Config) -> Result<Self, String> {
        let backend = std::env::var("STORAGE_BACKEND").unwrap_or_default();
        match backend.trim().to_ascii_lowercase().as_str() {
            "" | "local" => Self::local(&cfg.torrent_storage_path, &cfg.avatar_storage_path),
            "s3" => Self::s3(&S3Settings::from_vars(|k| std::env::var(k).ok())?),
            other => Err(format!("unknown STORAGE_BACKEND `{other}`; use local or s3")),
        }
    }

    /// Files on disk, in the given folders (created when missing).
    pub fn local(torrent_dir: &str, avatar_dir: &str) -> Result<Self, String> {
        let open = |dir: &str| -> Result<Arc<dyn ObjectStore>, String> {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir}: {e}"))?;
            let store = LocalFileSystem::new_with_prefix(dir).map_err(|e| format!("cannot open {dir}: {e}"))?;
            Ok(Arc::new(store))
        };
        Ok(Storage {
            torrents: open(torrent_dir)?,
            avatars: open(avatar_dir)?,
            description: format!("local disk ({torrent_dir}, {avatar_dir})"),
        })
    }

    pub fn s3(settings: &S3Settings) -> Result<Self, String> {
        // reqwest is built without a bundled TLS provider; use ring (see Cargo.toml)
        let _ = rustls::crypto::ring::default_provider().install_default();
        let store = settings.build()?;
        let under = |kind: Kind| -> Arc<dyn ObjectStore> {
            let prefix = match settings.prefix.as_str() {
                "" => kind.folder().to_string(),
                p => format!("{p}/{}", kind.folder()),
            };
            Arc::new(PrefixStore::new(store.clone(), prefix))
        };
        Ok(Storage {
            torrents: under(Kind::TorrentInfo),
            avatars: under(Kind::Avatar),
            description: format!(
                "S3 bucket {}{} at {}",
                settings.bucket,
                if settings.prefix.is_empty() { String::new() } else { format!(" (under {}/)", settings.prefix) },
                settings.endpoint.as_deref().unwrap_or("AWS")
            ),
        })
    }

    /// For the startup log line.
    pub fn description(&self) -> &str {
        &self.description
    }

    fn store(&self, kind: Kind) -> &Arc<dyn ObjectStore> {
        match kind {
            Kind::TorrentInfo => &self.torrents,
            Kind::Avatar => &self.avatars,
        }
    }

    /// Stores the file whole; readers see the old file or the new one, never part of one
    /// (local writes go to a temp file that is renamed into place).
    pub async fn put(&self, kind: Kind, id: i32, data: impl Into<Bytes>) -> object_store::Result<()> {
        self.put_key(kind, &kind.key(id), data.into()).await
    }

    async fn put_key(&self, kind: Kind, key: &Path, data: Bytes) -> object_store::Result<()> {
        self.store(kind).put(key, PutPayload::from_bytes(data)).await.map(|_| ())
    }

    /// The file, or None when it doesn't exist.
    pub async fn get(&self, kind: Kind, id: i32) -> object_store::Result<Option<Bytes>> {
        self.get_key(kind, &kind.key(id)).await
    }

    async fn get_key(&self, kind: Kind, key: &Path) -> object_store::Result<Option<Bytes>> {
        match self.store(kind).get(key).await {
            Ok(res) => res.bytes().await.map(Some),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Removes the file; a missing file is not an error.
    #[cfg(test)]
    pub async fn delete(&self, kind: Kind, id: i32) -> object_store::Result<()> {
        match self.store(kind).delete(&kind.key(id)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every stored key of `kind` with its size, skipping leftover temp files.
    pub async fn list(&self, kind: Kind) -> object_store::Result<Vec<(Path, u64)>> {
        let mut out: Vec<_> =
            self.store(kind).list(None).map_ok(|meta| (meta.location, meta.size)).try_collect().await?;
        out.retain(|(key, _)| !key.as_ref().ends_with(".tmp"));
        out.sort();
        Ok(out)
    }

    /// Copies every file from `self` to `dest`, skipping files `dest` already has with the
    /// same size. Returns (copied, skipped); with `dry_run` nothing is written.
    pub async fn copy_all(&self, dest: &Storage, dry_run: bool) -> Result<(usize, usize), String> {
        let (mut copied, mut skipped) = (0, 0);
        for kind in Kind::ALL {
            let have: std::collections::HashMap<Path, u64> = dest
                .list(kind)
                .await
                .map_err(|e| format!("listing {} in {}: {e}", kind.folder(), dest.description))?
                .into_iter()
                .collect();
            for (key, size) in
                self.list(kind).await.map_err(|e| format!("listing {} in {}: {e}", kind.folder(), self.description))?
            {
                if have.get(&key) == Some(&size) {
                    skipped += 1;
                    continue;
                }
                println!("{} {}/{key} ({size} bytes)", if dry_run { "would copy" } else { "copying" }, kind.folder());
                if !dry_run {
                    let data = self
                        .get_key(kind, &key)
                        .await
                        .map_err(|e| format!("reading {key}: {e}"))?
                        .ok_or_else(|| format!("{key} disappeared while copying"))?;
                    dest.put_key(kind, &key, data).await.map_err(|e| format!("writing {key}: {e}"))?;
                }
                copied += 1;
            }
        }
        Ok((copied, skipped))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A local store in a fresh temp folder; remove it with `std::fs::remove_dir_all(dir)`.
    pub fn temp_local(name: &str) -> (Storage, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("nyaa-storage-{}-{}", std::process::id(), name));
        std::fs::remove_dir_all(&dir).ok();
        let (t, a) = (dir.join("torrents"), dir.join("avatars"));
        (Storage::local(t.to_str().unwrap(), a.to_str().unwrap()).unwrap(), dir)
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn keys_match_the_old_disk_layout() {
        assert_eq!(Kind::TorrentInfo.key(12345).as_ref(), "12/12345.torrent.info");
        assert_eq!(Kind::TorrentInfo.key(7).as_ref(), "0/7.torrent.info");
        assert_eq!(Kind::Avatar.key(3).as_ref(), "3.png");
    }

    #[test]
    fn s3_settings_defaults_and_errors() {
        let s = S3Settings::from_vars(vars(&[
            ("S3_BUCKET", "nyaa"),
            ("S3_ENDPOINT", "http://localhost:3900/"),
            ("S3_PATH_STYLE", "true"),
            ("S3_PREFIX", "/site/"),
            ("S3_ACCESS_KEY", " "),
        ]))
        .unwrap();
        assert_eq!(
            s,
            S3Settings {
                endpoint: Some("http://localhost:3900".into()),
                bucket: "nyaa".into(),
                region: "us-east-1".into(),
                access_key: None,
                secret_key: None,
                path_style: true,
                prefix: "site".into(),
            }
        );
        assert!(S3Settings::from_vars(vars(&[])).unwrap_err().contains("S3_BUCKET"));
        assert!(S3Settings::from_vars(vars(&[("S3_BUCKET", "b"), ("S3_PATH_STYLE", "yes")])).is_err());
        assert!(Storage::s3(&s).is_ok());
    }

    #[actix_web::test]
    async fn local_put_get_delete_in_the_old_layout() {
        let (storage, dir) = temp_local("roundtrip");
        assert_eq!(storage.get(Kind::TorrentInfo, 1234).await.unwrap(), None);
        storage.put(Kind::TorrentInfo, 1234, b"d4:name1:ae".to_vec()).await.unwrap();
        storage.put(Kind::Avatar, 5, b"png".to_vec()).await.unwrap();
        assert_eq!(std::fs::read(dir.join("torrents/1/1234.torrent.info")).unwrap(), b"d4:name1:ae");
        assert_eq!(std::fs::read(dir.join("avatars/5.png")).unwrap(), b"png");
        assert_eq!(storage.get(Kind::Avatar, 5).await.unwrap().as_deref(), Some(&b"png"[..]));
        // Torrents and avatars don't see each other's files
        assert_eq!(storage.get(Kind::Avatar, 1234).await.unwrap(), None);

        std::fs::write(dir.join("avatars/6.png.tmp"), b"half").unwrap();
        let keys: Vec<String> = storage.list(Kind::Avatar).await.unwrap().iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(keys, ["5.png"]);

        storage.delete(Kind::Avatar, 5).await.unwrap();
        storage.delete(Kind::Avatar, 5).await.unwrap();
        assert_eq!(storage.get(Kind::Avatar, 5).await.unwrap(), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[actix_web::test]
    async fn copy_all_copies_new_and_changed_files_only() {
        let (src, src_dir) = temp_local("copy-src");
        let (dest, dest_dir) = temp_local("copy-dest");
        src.put(Kind::TorrentInfo, 1, b"one".to_vec()).await.unwrap();
        src.put(Kind::TorrentInfo, 2001, b"two".to_vec()).await.unwrap();
        src.put(Kind::Avatar, 1, b"avatar".to_vec()).await.unwrap();
        dest.put(Kind::TorrentInfo, 1, b"old".to_vec()).await.unwrap();
        dest.put(Kind::Avatar, 1, b"stale!".to_vec()).await.unwrap();

        assert_eq!(src.copy_all(&dest, true).await.unwrap(), (1, 2));
        assert_eq!(dest.get(Kind::TorrentInfo, 2001).await.unwrap(), None);
        assert_eq!(src.copy_all(&dest, false).await.unwrap(), (1, 2));
        assert_eq!(dest.get(Kind::TorrentInfo, 2001).await.unwrap().as_deref(), Some(&b"two"[..]));
        // Same size counts as already copied
        assert_eq!(dest.get(Kind::Avatar, 1).await.unwrap().as_deref(), Some(&b"stale!"[..]));
        assert_eq!(src.copy_all(&dest, false).await.unwrap(), (0, 3));
        std::fs::remove_dir_all(src_dir).ok();
        std::fs::remove_dir_all(dest_dir).ok();
    }

    /// Runs against a real S3-compatible server when NYAA_TEST_S3_BUCKET is set, with the
    /// other S3_* settings passed the same way (NYAA_TEST_S3_ENDPOINT and so on). CI runs it
    /// against Garage; see docker/README.md to run it locally.
    #[actix_web::test]
    async fn s3_roundtrip_when_configured() {
        let Ok(_) = std::env::var("NYAA_TEST_S3_BUCKET") else {
            eprintln!("skipped: NYAA_TEST_S3_BUCKET not set");
            return;
        };
        let mut settings = S3Settings::from_vars(|k| std::env::var(format!("NYAA_TEST_{k}")).ok()).unwrap();
        settings.prefix = format!("test-{}", std::process::id());
        let s3 = Storage::s3(&settings).unwrap();

        assert_eq!(s3.get(Kind::TorrentInfo, 42).await.unwrap(), None);
        s3.put(Kind::TorrentInfo, 42, b"d4:name1:ae".to_vec()).await.unwrap();
        s3.put(Kind::Avatar, 42, b"png".to_vec()).await.unwrap();
        assert_eq!(s3.get(Kind::TorrentInfo, 42).await.unwrap().as_deref(), Some(&b"d4:name1:ae"[..]));
        assert_eq!(s3.list(Kind::TorrentInfo).await.unwrap(), [(Kind::TorrentInfo.key(42), 11)]);

        // migrate-storage: local files land in the bucket under the same keys
        let (local, dir) = temp_local("s3-migrate");
        local.put(Kind::TorrentInfo, 1001, b"info".to_vec()).await.unwrap();
        local.put(Kind::Avatar, 42, b"png".to_vec()).await.unwrap();
        assert_eq!(local.copy_all(&s3, false).await.unwrap(), (1, 1));
        assert_eq!(s3.get(Kind::TorrentInfo, 1001).await.unwrap().as_deref(), Some(&b"info"[..]));

        for (kind, id) in [(Kind::TorrentInfo, 42), (Kind::TorrentInfo, 1001), (Kind::Avatar, 42)] {
            s3.delete(kind, id).await.unwrap();
            assert_eq!(s3.get(kind, id).await.unwrap(), None);
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
