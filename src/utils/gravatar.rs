//! Gravatar (or a Gravatar-compatible service such as Libravatar) behind `/avatar/{id}`.
//!
//! Upstream links straight to gravatar.com, so every page carries each commenter's
//! email hash, and an unsalted MD5 of an address is easy to reverse with a list of
//! known emails. Here the site fetches the image itself and serves it from its own
//! URL: the hash only travels from this server to GRAVATAR_URL (with a self-hosted
//! service it never leaves your network), and visitors' browsers never contact it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::config::Config;

/// How long a fetched image, or the service's "no avatar" answer, is reused.
const TTL: Duration = Duration::from_secs(3600);
/// How long to wait before asking again after the service failed.
const ERROR_TTL: Duration = Duration::from_secs(300);
/// Larger answers are refused; a 120px avatar is a few KB.
const MAX_BYTES: usize = 128 * 1024;
/// Cached hashes kept at most; the cache is cleared when it grows past this.
const MAX_ENTRIES: usize = 1000;

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub content_type: String,
    pub body: Bytes,
}

/// The email hash Gravatar-compatible services look up: MD5 (upstream) or SHA-256
/// of the trimmed, lowercased address.
pub fn email_hash(email: &str, sha256: bool) -> String {
    let email = email.trim().to_lowercase();
    if sha256 {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(email.as_bytes()))
    } else {
        use md5::{Digest, Md5};
        hex::encode(Md5::digest(email.as_bytes()))
    }
}

/// A cached answer: the image (None: the service has none) and until when it is reused.
struct Entry {
    fetched: Instant,
    ttl: Duration,
    image: Option<Image>,
}

pub struct GravatarProxy {
    client: reqwest::Client,
    cache: Mutex<HashMap<String, Entry>>,
}

impl GravatarProxy {
    pub fn new() -> Self {
        // reqwest is built without a bundled TLS provider; use ring (see Cargo.toml)
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(concat!("nyaa-actix/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("Failed to build the Gravatar HTTP client");
        GravatarProxy { client, cache: Mutex::new(HashMap::new()) }
    }

    /// The avatar for `email`, or None when the service has none (or failed).
    pub async fn fetch(&self, cfg: &Config, email: &str) -> Option<Image> {
        let hash = email_hash(email, cfg.gravatar_sha256);
        if let Some(entry) = self.cache.lock().unwrap().get(&hash) {
            if entry.fetched.elapsed() < entry.ttl {
                return entry.image.clone();
            }
        }
        // Nyaa: PG-rated, Sukebei: X-rated
        let rating = if cfg.site_flavor == "nyaa" { "pg" } else { "x" };
        let url = format!("{}/{}?s=120&d=404&r={}", cfg.gravatar_url, hash, rating);
        let (ttl, image) = match self.download(&url).await {
            Ok(image) => (TTL, image),
            Err(e) => {
                log::warn!("Gravatar request to {} failed: {e}", cfg.gravatar_url);
                (ERROR_TTL, None)
            }
        };
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_ENTRIES {
            cache.retain(|_, e| e.fetched.elapsed() < e.ttl);
            if cache.len() >= MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(hash, Entry { fetched: Instant::now(), ttl, image: image.clone() });
        image
    }

    async fn download(&self, url: &str) -> Result<Option<Image>, String> {
        let mut res = self.client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(format!("status {}", res.status()));
        }
        let content_type =
            res.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        // Only plain raster images; never pass SVG or HTML through as the site's own content.
        if !matches!(content_type.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp") {
            return Err(format!("unexpected content type {content_type:?}"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(|e| e.without_url().to_string())? {
            if body.len() + chunk.len() > MAX_BYTES {
                return Err("image too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(Image { content_type, body: body.into() }))
    }
}

impl Default for GravatarProxy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A tiny HTTP server answering every request with `status`, `content_type` and `body`.
    /// Returns its base URL, the request paths it saw and a hit counter.
    pub fn fake_service(
        status: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/avatar", listener.local_addr().unwrap());
        let (paths, hits) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)));
        let (p, h) = (paths.clone(), hits.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                p.lock().unwrap().push(line.split_whitespace().nth(1).unwrap_or("").to_string());
                while {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap() > 2
                } {}
                h.fetch_add(1, Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (base, paths, hits)
    }

    #[test]
    fn hashes_match_gravatar_and_libravatar() {
        assert_eq!(email_hash(" Alice@Example.com ", false), "c160f8cc69a4f0bf2b0362752353d060");
        assert_eq!(
            email_hash("alice@example.com", true),
            "ff8d9819fc0e12bf0d24892e45987e249a28dce836a85cad60e28eaaa8c6d976"
        );
    }

    #[actix_web::test]
    async fn fetches_once_and_caches() {
        let (base, paths, hits) = fake_service("200 OK", "image/png", b"png".to_vec());
        let mut cfg = crate::handlers::account::tests::config("gravatar-proxy-cache");
        cfg.gravatar_url = base;
        let proxy = GravatarProxy::new();
        let image = proxy.fetch(&cfg, "alice@example.com").await.unwrap();
        assert_eq!(image, Image { content_type: "image/png".into(), body: Bytes::from_static(b"png") });
        assert!(proxy.fetch(&cfg, "ALICE@example.com").await.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(paths.lock().unwrap()[0], "/avatar/c160f8cc69a4f0bf2b0362752353d060?s=120&d=404&r=pg");
    }

    #[actix_web::test]
    async fn missing_and_unsafe_answers_give_none() {
        let mut cfg = crate::handlers::account::tests::config("gravatar-proxy-none");
        for (status, content_type) in
            [("404 Not Found", "image/png"), ("200 OK", "image/svg+xml"), ("500 Oops", "image/png")]
        {
            cfg.gravatar_url = fake_service(status, content_type, b"x".to_vec()).0;
            assert_eq!(GravatarProxy::new().fetch(&cfg, "alice@example.com").await, None, "{status} {content_type}");
        }
        cfg.gravatar_url = fake_service("200 OK", "image/png", vec![0; MAX_BYTES + 1]).0;
        assert_eq!(GravatarProxy::new().fetch(&cfg, "alice@example.com").await, None);
    }
}
