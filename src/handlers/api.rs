//! Upstream's JSON API: `/api/info/<id or hash>` and `/api/upload` (also at
//! `/api/v2/upload`), with the same fields so its client scripts work unchanged. Requests
//! sign in with HTTP Basic auth (username or email, and password) and never use the
//! session cookie, which is why the CSRF guard leaves `/api/` alone. No answer carries
//! `WWW-Authenticate`, so browsers never remember the credentials and send them along
//! with a cross-site form.

use std::collections::HashMap;

use actix_multipart::Multipart;
use actix_web::http::{header, StatusCode};
use actix_web::{web, HttpRequest, HttpResponse, Result};
use base64::Engine;
use diesel::prelude::*;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::auth::Permission;
use crate::config::Config;
use crate::db::schema::{nyaa_main_categories, nyaa_statistics};
use crate::db::DbPool;
use crate::handlers::account::{LOGIN_FAILURES_BY_ACCOUNT, LOGIN_FAILURES_BY_IP};
use crate::handlers::torrents::{create_torrent, read_field, EditForm, Upload, MAX_TORRENT_SIZE};
use crate::models::{password_matches, MainCategory, Statistic, Torrent, User};
use crate::storage::{Kind, Storage};
use crate::torrent::FileNode;
use crate::utils::{client_ip, internal_error};

fn error(status: StatusCode, errors: serde_json::Value) -> HttpResponse {
    HttpResponse::build(status).json(json!({ "errors": errors }))
}

/// The user named by the request's Basic auth, or the error to answer with. Failed
/// passwords count towards the same limits as the login form.
/// The error response is boxed: `HttpResponse` is large.
async fn api_user(req: &HttpRequest, pool: &DbPool) -> std::result::Result<User, Box<HttpResponse>> {
    let bad = || Box::new(error(StatusCode::FORBIDDEN, json!(["Bad authorization"])));
    let credentials = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v.trim()).ok())
        .and_then(|v| String::from_utf8(v).ok());
    let Some((username, password)) = credentials.as_deref().and_then(|c| c.split_once(':')) else {
        return Err(bad());
    };

    let mut conn = pool.get().map_err(|e| Box::new(HttpResponse::from_error(internal_error(e))))?;
    let user = User::by_username_or_email(&mut conn, username)
        .map_err(|e| Box::new(HttpResponse::from_error(internal_error(e))))?;
    drop(conn);

    let ip = crate::handlers::account::client_key(req);
    let account = match &user {
        Some(u) => format!("user:{}", u.id),
        None => format!("name:{}", username.trim().to_lowercase()),
    };
    if LOGIN_FAILURES_BY_IP.is_blocked(&ip) || LOGIN_FAILURES_BY_ACCOUNT.is_blocked(&account) {
        return Err(Box::new(error(
            StatusCode::TOO_MANY_REQUESTS,
            json!(["Too many failed login attempts. Try again in 15 minutes."]),
        )));
    }
    let (checked, password) = (user.clone(), password.to_string());
    let password_ok = web::block(move || password_matches(checked.as_ref(), &password))
        .await
        .map_err(|e| Box::new(HttpResponse::from_error(internal_error(e))))?;
    match user {
        Some(u) if password_ok => {
            LOGIN_FAILURES_BY_ACCOUNT.clear(&account);
            if u.is_banned() {
                Err(Box::new(error(StatusCode::FORBIDDEN, json!(["Your account has been banned."]))))
            } else if !u.is_active() {
                Err(Box::new(error(StatusCode::FORBIDDEN, json!(["Your account is not active."]))))
            } else {
                Ok(u)
            }
        }
        _ => {
            LOGIN_FAILURES_BY_IP.hit(&ip);
            LOGIN_FAILURES_BY_ACCOUNT.hit(&account);
            Err(Box::new(error(StatusCode::FORBIDDEN, json!(["Incorrect username or password"]))))
        }
    }
}

/// Unwraps [`api_user`] inside a handler.
macro_rules! api_user {
    ($req:expr, $pool:expr) => {
        match api_user($req, $pool).await {
            Ok(user) => user,
            Err(response) => return Ok(*response),
        }
    };
}

/// `torrent_data` in an upload: upstream's keys and defaults.
#[derive(Default, Deserialize)]
#[serde(default)]
struct UploadData {
    name: Option<String>,
    category: Option<String>,
    anonymous: Option<bool>,
    hidden: Option<bool>,
    complete: Option<bool>,
    remake: Option<bool>,
    trusted: Option<bool>,
    information: Option<String>,
    description: Option<String>,
}

impl UploadData {
    /// The web form's fields; `null` counts as left out. Trusted defaults to on, as
    /// upstream, and only sticks for users who may set it.
    fn into_form(self) -> EditForm {
        EditForm {
            display_name: self.name.unwrap_or_default(),
            category: self.category.unwrap_or_default(),
            information: self.information.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            is_anonymous: self.anonymous.unwrap_or(false),
            is_hidden: self.hidden.unwrap_or(false),
            is_complete: self.complete.unwrap_or(false),
            is_remake: self.remake.unwrap_or(false),
            is_trusted: self.trusted.unwrap_or(true),
            ..Default::default()
        }
    }
}

/// Upload form field names as the API calls them.
fn api_field(field: &str) -> &str {
    match field {
        "torrent_file" => "torrent",
        "display_name" => "name",
        other => other,
    }
}

/// `POST /api/upload` and `/api/v2/upload`: multipart with the .torrent as `torrent` and
/// the details as JSON in `torrent_data`. Answers with the new torrent's url, id, name,
/// hash and magnet, or 400 and `{"errors": {"field": ["message"]}}`.
pub async fn upload(
    req: HttpRequest,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    storage: web::Data<Storage>,
    mut payload: Multipart,
) -> Result<HttpResponse> {
    let user = api_user!(&req, &pool);

    let (mut torrent_file, mut torrent_data) = (None, None);
    let mut fields = 0;
    while let Some(item) = payload.next().await {
        let mut field = item.map_err(actix_web::error::ErrorBadRequest)?;
        fields += 1;
        if fields > 8 {
            return Err(actix_web::error::ErrorPayloadTooLarge("Too many form fields"));
        }
        match field.name().unwrap_or_default() {
            "torrent" => torrent_file = Some(read_field(&mut field, MAX_TORRENT_SIZE).await?),
            // Room for a full description of escaped non-ASCII text
            "torrent_data" => torrent_data = Some(read_field(&mut field, 256 * 1024).await?),
            _ => {
                read_field(&mut field, MAX_TORRENT_SIZE).await?;
            }
        }
    }
    let Some(torrent_data) = torrent_data else {
        return Ok(error(StatusCode::BAD_REQUEST, json!(["missing torrent_data field"])));
    };
    let Ok(data) = serde_json::from_slice::<UploadData>(&torrent_data) else {
        return Ok(error(StatusCode::BAD_REQUEST, json!(["unable to parse valid JSON in torrent_data"])));
    };

    let upload = Upload { torrent_file, form: data.into_form(), group_id: None };
    match create_torrent(&pool, &cfg, &storage, &user, client_ip(&req), &upload).await? {
        Ok(t) => Ok(HttpResponse::Ok().json(json!({
            "url": format!("{}/view/{}", cfg.site_url, t.id),
            "id": t.id,
            "name": t.display_name,
            "hash": t.info_hash_hex(),
            "magnet": t.magnet_uri(&t.display_name, &cfg.trackers()),
        }))),
        Err(errors) => {
            let errors: HashMap<&str, Vec<String>> =
                errors.into_iter().map(|(field, message)| (api_field(field), vec![message])).collect();
            Ok(error(StatusCode::BAD_REQUEST, json!(errors)))
        }
    }
}

/// `GET /api/info/<id or hex hash>`: a torrent's details, as upstream's `v2_api_info`.
pub async fn info(
    req: HttpRequest,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    storage: web::Data<Storage>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let viewer = api_user!(&req, &pool);
    let invalid = || Ok(error(StatusCode::BAD_REQUEST, json!(["Query was not a valid id or hash."])));

    let query = path.into_inner().trim().to_ascii_lowercase();
    let mut conn = pool.get().map_err(internal_error)?;
    let torrent = if !query.is_empty() && query.bytes().all(|b| b.is_ascii_digit()) {
        match query.parse() {
            Ok(id) => Torrent::by_id(&mut conn, id).map_err(internal_error)?,
            Err(_) => None,
        }
    } else if query.len() == 40 {
        match hex::decode(&query) {
            Ok(hash) => Torrent::by_info_hash(&mut conn, &hash).map_err(internal_error)?,
            Err(_) => return invalid(),
        }
    } else {
        return invalid();
    };
    // Deleted and banned torrents only exist for moderators, as on the site
    let moderator = viewer.can(Permission::ModerateTorrents);
    let Some(t) = torrent.filter(|t| moderator || !(t.is_deleted() || t.is_banned())) else {
        return invalid();
    };

    let submitter = match t.uploader_id {
        Some(uid) if !t.is_anonymous() || moderator || viewer.id == uid => {
            User::by_id(&mut conn, uid).map_err(internal_error)?.map(|u| u.username)
        }
        _ => None,
    };
    let main_category = nyaa_main_categories::table
        .find(t.main_category_id)
        .first::<MainCategory>(&mut conn)
        .optional()
        .map_err(internal_error)?;
    let sub_category =
        crate::models::get_sub_category(&mut conn, t.main_category_id, t.sub_category_id).map_err(internal_error)?;
    let stats = nyaa_statistics::table.find(t.id).first::<Statistic>(&mut conn).optional().map_err(internal_error)?;
    drop(conn);

    let info = storage.get(Kind::TorrentInfo, t.id).await.unwrap_or_else(|e| {
        log::warn!("Reading info dict of torrent {}: {e}", t.id);
        None
    });
    let files = info.and_then(|info| crate::torrent::file_tree(&info)).map_or(json!({}), |(tree, _)| file_map(&tree));

    Ok(HttpResponse::Ok().json(json!({
        "submitter": submitter,
        "url": format!("{}/view/{}", cfg.site_url, t.id),
        "id": t.id,
        "name": t.display_name,
        "creation_date": t.created_time.format("%Y-%m-%d %H:%M").to_string(),
        "hash_b32": base32(&t.info_hash),
        "hash_hex": t.info_hash_hex(),
        "magnet": t.magnet_uri(&t.display_name, &cfg.trackers()),
        "main_category": main_category.map(|c| c.name),
        "main_category_id": t.main_category_id,
        "sub_category": sub_category.map(|c| c.name),
        "sub_category_id": t.sub_category_id,
        "information": t.information,
        "description": t.description,
        "stats": {
            "seeders": stats.as_ref().map_or(0, |s| s.seed_count),
            "leechers": stats.as_ref().map_or(0, |s| s.leech_count),
            "downloads": stats.as_ref().map_or(0, |s| s.download_count),
        },
        "filesize": t.filesize,
        "files": files,
        "is_trusted": t.is_trusted(),
        "is_complete": t.is_complete(),
        "is_remake": t.is_remake(),
    })))
}

/// Upstream's file list: folders are objects, files are their sizes.
fn file_map(nodes: &[FileNode]) -> serde_json::Value {
    let map = nodes
        .iter()
        .map(|n| {
            let value = match n.size {
                Some(size) if n.children.is_empty() => json!(size),
                _ => file_map(&n.children),
            };
            (n.name.clone(), value)
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::Value::Object(map)
}

/// RFC 4648 base32 without padding, as in magnet URIs (upstream's `info_hash_as_b32`).
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut out, mut buf, mut bits) = (String::new(), 0u32, 0u32);
    for &b in bytes {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
        buf &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{cookie::Key, test as atest, App};
    use diesel::r2d2::Pool;
    use tera::Tera;

    #[test]
    fn base32_matches_magnet_hashes() {
        assert_eq!(
            base32(&hex::decode("0123456789abcdef0123456789abcdef01234567").unwrap()),
            "AERUKZ4JVPG66AJDIVTYTK6N54ASGRLH"
        );
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn file_lists_nest_folders() {
        let info = b"d5:filesld6:lengthi3e4:pathl1:a5:x.mkveed6:lengthi4e4:pathl5:y.txteee4:name4:root12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let (tree, _) = crate::torrent::file_tree(info).unwrap();
        assert_eq!(file_map(&tree), json!({"root": {"a": {"x.mkv": 3}, "y.txt": 4}}));
        let single = b"d6:lengthi5e4:name5:c.txt12:piece lengthi16384e6:pieces20:CCCCCCCCCCCCCCCCCCCCe";
        assert_eq!(file_map(&crate::torrent::file_tree(single).unwrap().0), json!({"c.txt": 5}));
    }

    fn torrent_file(name: &str) -> Vec<u8> {
        let mut file =
            format!("d4:infod6:lengthi5e4:name{}:{name}12:piece lengthi16384e6:pieces20:", name.len()).into_bytes();
        file.extend_from_slice(&[b'P'; 20]);
        file.extend_from_slice(b"ee");
        file
    }

    fn body(data: Option<&str>, file: Option<&[u8]>) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some(data) = data {
            body.extend_from_slice(
                format!("--XX\r\nContent-Disposition: form-data; name=\"torrent_data\"\r\n\r\n{data}\r\n").as_bytes(),
            );
        }
        if let Some(file) = file {
            body.extend_from_slice(
                b"--XX\r\nContent-Disposition: form-data; name=\"torrent\"; filename=\"a.torrent\"\r\n\r\n",
            );
            body.extend_from_slice(file);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"--XX--\r\n");
        body
    }

    fn basic(user: &str, password: &str) -> (header::HeaderName, String) {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        (header::AUTHORIZATION, format!("Basic {token}"))
    }

    /// Uploads through the API, reads them back through /api/info and the RSS feed.
    #[actix_web::test]
    async fn upload_info_and_rss_round_trip() {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        {
            let mut conn = pool.get().unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            let hash = crate::models::hash_password("secret1");
            // Failed logins also count per account id, process-wide: ids no other test uses
            diesel::sql_query(format!(
                "INSERT INTO users (id, username, email, password_hash, status, level) VALUES \
                 (901, 'up', 'up@example.com', '{hash}', 1, 0), (902, 'mod', NULL, '{hash}', 1, 2), \
                 (903, 'gone', NULL, '{hash}', 2, 0), (904, 'other', NULL, '{hash}', 1, 0)"
            ))
            .execute(&mut conn)
            .unwrap();
        }
        let dir = std::env::temp_dir().join(format!("nyaa-api-test-{}", std::process::id()));
        let dir = dir.to_str().unwrap().to_string();
        let storage = Storage::local(&dir, &dir).unwrap();
        let cfg = Config {
            site_url: "https://nyaa.example".into(),
            tracker_urls: vec!["udp://t.example:6969/announce".into()],
            ..Config::for_tests()
        };
        let mut tera = Tera::new("templates/**/*").unwrap();
        crate::utils::tera_filters::register(&mut tera);
        let app = atest::init_service(
            App::new()
                .app_data(web::Data::new(cfg))
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(storage))
                .app_data(web::Data::new(tera))
                .wrap(actix_web::middleware::from_fn(crate::middleware::csrf::reject_cross_site))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/", web::get().to(crate::handlers::home::home))
                .route("/rss", web::get().to(crate::handlers::home::rss))
                .route("/api/info/{query}", web::get().to(info))
                .route("/api/upload", web::post().to(upload))
                .route("/api/v2/upload", web::post().to(upload)),
        )
        .await;
        // Scripts send no Origin; the CSRF guard lets /api/ through
        let post = |uri: &str, auth: Option<(&str, &str)>, body: Vec<u8>| {
            // Failed logins count per address, process-wide; keep this test's apart from
            // the login tests'
            let mut req = atest::TestRequest::post()
                .peer_addr("192.0.2.77:1000".parse().unwrap())
                .uri(uri)
                .insert_header(("Content-Type", "multipart/form-data; boundary=XX"))
                .set_payload(body);
            if let Some((u, p)) = auth {
                req = req.insert_header(basic(u, p));
            }
            req.to_request()
        };
        let call = |req| async {
            let res = atest::call_service(&app, req).await;
            let status = res.status().as_u16();
            let bytes = atest::read_body(res).await;
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
            )
        };

        let data = r##"{"name": "Show - 01", "category": "1_2", "anonymous": true, "information": "#x@irc"}"##;
        let file = torrent_file("a.mkv");
        assert_eq!(
            call(post("/api/upload", None, body(Some(data), Some(&file)))).await,
            (403, json!({"errors": ["Bad authorization"]}))
        );
        assert_eq!(
            call(post("/api/upload", Some(("up", "wrong")), body(Some(data), Some(&file)))).await,
            (403, json!({"errors": ["Incorrect username or password"]}))
        );
        assert_eq!(call(post("/api/upload", Some(("gone", "secret1")), body(Some(data), Some(&file)))).await.0, 403);
        assert_eq!(
            call(post("/api/upload", Some(("up", "secret1")), body(None, Some(&file)))).await,
            (400, json!({"errors": ["missing torrent_data field"]}))
        );
        assert_eq!(
            call(post("/api/upload", Some(("up", "secret1")), body(Some("{nope"), Some(&file)))).await,
            (400, json!({"errors": ["unable to parse valid JSON in torrent_data"]}))
        );
        let (status, errors) = call(post(
            "/api/upload",
            Some(("up", "secret1")),
            body(Some(r#"{"name": "ab", "category": "1_0"}"#), None),
        ))
        .await;
        assert_eq!(status, 400);
        assert_eq!(errors["errors"]["torrent"], json!(["Please select a torrent file."]));
        assert_eq!(errors["errors"]["category"], json!(["Please select a proper category"]));
        assert!(errors["errors"]["name"][0].as_str().unwrap().contains("at least 3"));

        // By email, at the v2 URL
        let (status, created) =
            call(post("/api/v2/upload", Some(("up@example.com", "secret1")), body(Some(data), Some(&file)))).await;
        assert_eq!(status, 200, "{created}");
        let id = created["id"].as_i64().unwrap();
        let hash = created["hash"].as_str().unwrap().to_string();
        assert_eq!(created["url"], format!("https://nyaa.example/view/{id}"));
        assert_eq!(created["name"], "Show - 01");
        assert!(created["magnet"]
            .as_str()
            .unwrap()
            .starts_with(&format!("magnet:?xt=urn:btih:{}", hash.to_uppercase())));
        let t = Torrent::by_id(&mut pool.get().unwrap(), id as i32).unwrap().unwrap();
        // Trusted defaults to on but needs a trusted uploader
        assert!(t.is_anonymous() && !t.is_trusted() && !t.is_hidden());
        assert_eq!(t.uploader_id, Some(901));

        let (status, dup) = call(post("/api/upload", Some(("up", "secret1")), body(Some(data), Some(&file)))).await;
        assert_eq!(
            (status, dup["errors"]["torrent"][0].as_str().unwrap()),
            (400, format!("This torrent already exists (#{id})").as_str())
        );

        // Info by id and by hash; the anonymous uploader shows to themselves and moderators only
        let get = |uri: String, user: &str| {
            atest::TestRequest::get().uri(&uri).insert_header(basic(user, "secret1")).to_request()
        };
        let (status, by_id) = call(get(format!("/api/info/{id}"), "other")).await;
        assert_eq!(status, 200);
        assert_eq!(by_id["submitter"], serde_json::Value::Null);
        assert_eq!(by_id["hash_hex"], hash);
        assert_eq!(by_id["hash_b32"], base32(&hex::decode(&hash).unwrap()));
        assert_eq!((by_id["main_category_id"].clone(), by_id["sub_category_id"].clone()), (json!(1), json!(2)));
        assert_eq!(by_id["main_category"], "Anime");
        assert_eq!(by_id["information"], "#x@irc");
        assert_eq!(by_id["files"], json!({"a.mkv": 5}));
        assert_eq!(by_id["stats"], json!({"seeders": 0, "leechers": 0, "downloads": 0}));
        assert_eq!((by_id["is_trusted"].clone(), by_id["is_remake"].clone()), (json!(false), json!(false)));
        let (_, by_hash) = call(get(format!("/api/info/{}", hash.to_uppercase()), "mod")).await;
        assert_eq!((by_hash["id"].as_i64(), by_hash["submitter"].as_str()), (Some(id), Some("up")));
        assert_eq!(call(get(format!("/api/info/{id}"), "up")).await.1["submitter"], "up");
        for bad in ["999", "xyz", &"0".repeat(40)] {
            assert_eq!(
                call(get(format!("/api/info/{bad}"), "up")).await,
                (400, json!({"errors": ["Query was not a valid id or hash."]}))
            );
        }
        assert_eq!(call(atest::TestRequest::get().uri(&format!("/api/info/{id}")).to_request()).await.0, 403);

        // The feed lists it with upstream's nyaa: fields
        macro_rules! feed {
            ($uri:expr) => {{
                let res = atest::call_service(&app, atest::TestRequest::get().uri($uri).to_request()).await;
                let status = res.status().as_u16();
                (status, String::from_utf8(atest::read_body(res).await.to_vec()).unwrap())
            }};
        }
        let (status, xml) = feed!("/?page=rss");
        assert_eq!(status, 200);
        for want in [
            "<title>Nyaa - Home - Torrent File RSS</title>",
            "xmlns:nyaa=\"https://nyaa.example/xmlns/nyaa\"",
            "<title>Show - 01</title>",
            &format!("<link>https://nyaa.example/download/{id}.torrent</link>"),
            &format!("<guid isPermaLink=\"true\">https://nyaa.example/view/{id}</guid>"),
            &format!("<nyaa:infoHash>{hash}</nyaa:infoHash>"),
            "<nyaa:categoryId>1_2</nyaa:categoryId>",
            "<nyaa:size>5 Bytes</nyaa:size>",
            "<nyaa:trusted>No</nyaa:trusted>",
        ] {
            assert!(xml.contains(want), "{want}\n{xml}");
        }
        let (_, xml) = feed!("/rss?term=show&m");
        assert!(xml.contains("<title>Nyaa - &quot;show&quot; - Magnet URI RSS</title>"), "{xml}");
        assert!(xml.contains("<link>magnet:?xt=urn:btih:"), "{xml}");
        assert!(xml.contains("&amp;tr=udp%3A%2F%2Ft.example"), "{xml}");
        assert!(!feed!("/?page=rss&q=nothing-like-it").1.contains("<item>"));
        // Anonymous uploads don't show under their uploader's name, as on profiles
        assert!(!feed!("/?page=rss&u=up").1.contains("<item>"));
        assert_eq!(feed!("/?page=rss&user=nobody").0, 404);
        std::fs::remove_dir_all(dir).ok();
    }
}
