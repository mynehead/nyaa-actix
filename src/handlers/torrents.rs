use actix_multipart::Multipart;
use actix_session::Session;
use actix_web::http::header::{Charset, ContentDisposition, DispositionParam, DispositionType, ExtendedValue};
use actix_web::{web, HttpRequest, HttpResponse, Result};
use futures_util::StreamExt;
use tera::Tera;
use std::path::PathBuf;
use diesel::prelude::*;

use crate::config::Config;
use crate::db::DbPool;
use crate::db::schema::{nyaa_torrents, nyaa_statistics, nyaa_comments};
use crate::middleware::auth::get_current_user;
use crate::models::{NewTorrent, NewStatistic, Torrent, User};
use crate::torrent::{parse_torrent, rebuild_torrent};
use crate::utils::{pack_ip, sanitize_string};

pub async fn view_torrent(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let current_user = get_current_user(&session, &pool);
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;

    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;

    check_visible(&torrent, &current_user)?;

    let stats = nyaa_statistics::table
        .find(torrent_id)
        .first::<crate::models::Statistic>(&mut conn)
        .optional()
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let comments: Vec<crate::models::Comment> = nyaa_comments::table
        .filter(nyaa_comments::torrent_id.eq(torrent_id))
        .order(nyaa_comments::created_time.asc())
        .load(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    // Anonymous uploads only name their uploader to that uploader and moderators
    let can_see_uploader = !torrent.is_anonymous() || current_user.as_ref()
        .map(|u| u.is_moderator() || Some(u.id) == torrent.uploader_id)
        .unwrap_or(false);
    let uploader: Option<User> = match torrent.uploader_id {
        Some(uid) if can_see_uploader => User::by_id(&mut conn, uid)
            .map_err(actix_web::error::ErrorInternalServerError)?,
        _ => None,
    };

    let magnet = torrent.magnet_uri(&torrent.display_name, &cfg.trackers());

    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &current_user);
    ctx.insert("torrent", &torrent);
    ctx.insert("stats", &stats);
    ctx.insert("comments", &comments);
    ctx.insert("uploader", &uploader);
    ctx.insert("magnet", &magnet);
    ctx.insert("config", &serde_json::json!({
        "site_name": cfg.site_name,
        "site_flavor": cfg.site_flavor,
    }));

    let html = tmpl.render("torrent.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// Deleted and banned torrents are only visible to moderators.
fn check_visible(torrent: &Torrent, current_user: &Option<User>) -> Result<()> {
    let is_admin = current_user.as_ref().map(|u| u.is_moderator()).unwrap_or(false);
    if (torrent.is_deleted() || torrent.is_banned()) && !is_admin {
        return Err(actix_web::error::ErrorNotFound("Torrent not found"));
    }
    Ok(())
}

pub async fn download_torrent(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let current_user = get_current_user(&session, &pool);
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;

    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    check_visible(&torrent, &current_user)?;

    if torrent.has_torrent == 0 {
        return Err(actix_web::error::ErrorNotFound("Torrent file not available"));
    }

    let path: PathBuf = [&cfg.torrent_storage_path,
        &format!("{}", torrent_id / 1000),
        &format!("{}.torrent.info", torrent_id)
    ].iter().collect();

    let bencoded_info = std::fs::read(&path)
        .map_err(|_| actix_web::error::ErrorNotFound("Torrent file not found"))?;

    let torrent_data = rebuild_torrent(&torrent, &bencoded_info, &cfg.trackers(), &cfg.site_url);

    Ok(HttpResponse::Ok()
        .content_type("application/x-bittorrent")
        .insert_header(attachment_header(&torrent.torrent_name))
        .body(torrent_data))
}

/// `attachment` with an ASCII `filename` fallback plus the UTF-8 `filename*`,
/// so the uploader-controlled name can't break out of the header.
fn attachment_header(name: &str) -> ContentDisposition {
    let ascii: String = name.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '_' })
        .collect();
    ContentDisposition {
        disposition: DispositionType::Attachment,
        parameters: vec![
            DispositionParam::Filename(ascii),
            DispositionParam::FilenameExt(ExtendedValue {
                charset: Charset::Ext("UTF-8".into()),
                language_tag: None,
                value: name.as_bytes().to_vec(),
            }),
        ],
    }
}

/// Makes a .torrent `name` safe to offer as a download filename.
fn torrent_filename(name: &str) -> String {
    let cleaned: String = sanitize_string(name).chars()
        .map(|c| if matches!(c, '"' | '/' | '\\') { '_' } else { c })
        .collect();
    format!("{}.torrent", cleaned.trim())
}

pub async fn magnet_redirect(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let current_user = get_current_user(&session, &pool);
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    check_visible(&torrent, &current_user)?;
    let magnet = torrent.magnet_uri(&torrent.display_name, &cfg.trackers());
    Ok(HttpResponse::Found()
        .insert_header(("Location", magnet))
        .finish())
}

pub async fn upload_get(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    if current_user.is_none() {
        return Ok(HttpResponse::Found()
            .insert_header(("Location", "/account/login"))
            .finish());
    }
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let categories = crate::models::get_all_categories(&mut conn)
        .map_err(actix_web::error::ErrorInternalServerError)?;

    let groups: Vec<crate::models::Group> = if let Some(ref user) = current_user {
        crate::models::Group::all(&mut conn)
            .map_err(actix_web::error::ErrorInternalServerError)?
            .into_iter()
            .filter(|g| g.can_upload(&mut conn, user.id))
            .collect()
    } else {
        vec![]
    };

    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &current_user);
    ctx.insert("categories", &categories);
    ctx.insert("groups", &groups);
    ctx.insert("config", &serde_json::json!({
        "site_name": cfg.site_name,
        "site_flavor": cfg.site_flavor,
    }));
    let html = tmpl.render("upload.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// nyaa's limit for .torrent files.
const MAX_TORRENT_SIZE: usize = 10 * 1024 * 1024;
/// Per text field (name, information, description, ...).
const MAX_TEXT_FIELD_SIZE: usize = 64 * 1024;
/// Well above what the upload form sends.
const MAX_FIELDS: usize = 32;

/// Reads one multipart field, failing with 413 as soon as it passes `limit`
/// so a huge field is never buffered in memory.
async fn read_field(field: &mut actix_multipart::Field, limit: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    while let Some(chunk) = field.next().await {
        let chunk = chunk.map_err(actix_web::error::ErrorBadRequest)?;
        if data.len() + chunk.len() > limit {
            return Err(actix_web::error::ErrorPayloadTooLarge("Upload field too large"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

pub async fn upload_post(
    req: HttpRequest,
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    mut payload: Multipart,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    if current_user.is_none() {
        return Err(actix_web::error::ErrorUnauthorized("Login required"));
    }
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;

    let mut torrent_bytes: Option<Vec<u8>> = None;
    let mut display_name = String::new();
    let mut information = String::new();
    let mut description = String::new();
    let mut category = String::new();
    let mut group_id: Option<i32> = None;
    let mut flags: i32 = 0;

    let mut field_count = 0;
    while let Some(item) = payload.next().await {
        let mut field = item.map_err(actix_web::error::ErrorBadRequest)?;
        field_count += 1;
        if field_count > MAX_FIELDS {
            return Err(actix_web::error::ErrorPayloadTooLarge("Too many form fields"));
        }
        let name = field.name().unwrap_or_default().to_string();
        let limit = if name == "torrent_file" { MAX_TORRENT_SIZE } else { MAX_TEXT_FIELD_SIZE };
        let data = read_field(&mut field, limit).await?;
        match name.as_str() {
            "torrent_file" => torrent_bytes = Some(data),
            "display_name" => display_name = String::from_utf8_lossy(&data).into_owned(),
            "information" => information = String::from_utf8_lossy(&data).into_owned(),
            "description" => description = String::from_utf8_lossy(&data).into_owned(),
            "category" => category = String::from_utf8_lossy(&data).into_owned(),
            "group_id" => group_id = String::from_utf8_lossy(&data).parse().ok(),
            "is_hidden" => { if !data.is_empty() { flags |= crate::models::TorrentFlags::HIDDEN.bits(); } }
            "is_remake" => { if !data.is_empty() { flags |= crate::models::TorrentFlags::REMAKE.bits(); } }
            "is_anonymous" => { if !data.is_empty() { flags |= crate::models::TorrentFlags::ANONYMOUS.bits(); } }
            "is_complete" => { if !data.is_empty() { flags |= crate::models::TorrentFlags::COMPLETE.bits(); } }
            "is_trusted" => {
                if !data.is_empty() {
                    if current_user.as_ref().map(|u| u.is_trusted()).unwrap_or(false) {
                        flags |= crate::models::TorrentFlags::TRUSTED.bits();
                    }
                }
            }
            _ => {}
        }
    }

    let torrent_bytes = torrent_bytes
        .ok_or_else(|| actix_web::error::ErrorBadRequest("No torrent file uploaded"))?;

    let meta = parse_torrent(&torrent_bytes)
        .map_err(|e| actix_web::error::ErrorBadRequest(format!("Invalid torrent: {}", e)))?;

    // Check duplicate
    if let Some(_existing) = Torrent::by_info_hash(&mut conn, &meta.info_hash)
        .map_err(actix_web::error::ErrorInternalServerError)?
    {
        return Err(actix_web::error::ErrorBadRequest("This torrent already exists"));
    }

    // Parse category
    let parts: Vec<&str> = category.splitn(2, '_').collect();
    let (main_cat, sub_cat) = if parts.len() == 2 {
        (parts[0].parse::<i32>().unwrap_or(1), parts[1].parse::<i32>().unwrap_or(0))
    } else {
        (1, 0)
    };

    // Validate group permission
    let resolved_group: Option<i32> = if let (Some(gid), Some(ref user)) = (group_id, &current_user) {
        let grp = crate::models::Group::by_id(&mut conn, gid)
            .map_err(actix_web::error::ErrorInternalServerError)?;
        if let Some(g) = grp {
            if g.can_upload(&mut conn, user.id) { Some(gid) } else { None }
        } else { None }
    } else { None };

    let final_name = if display_name.trim().is_empty() {
        sanitize_string(&meta.display_name)
    } else {
        sanitize_string(display_name.trim())
    };

    let now = chrono::Utc::now().naive_utc();

    let new_torrent = NewTorrent {
        info_hash: meta.info_hash.clone(),
        display_name: final_name,
        torrent_name: torrent_filename(&meta.display_name),
        information: sanitize_string(information.trim()),
        description: sanitize_string(description.trim()),
        filesize: meta.filesize,
        encoding: meta.encoding.clone(),
        flags,
        uploader_id: current_user.as_ref().map(|u| u.id),
        uploader_ip: req.peer_addr().map(|a| pack_ip(a.ip())),
        has_torrent: 1,
        comment_count: 0,
        created_time: now,
        updated_time: now,
        main_category_id: main_cat,
        sub_category_id: sub_cat,
        group_id: resolved_group,
    };

    // One transaction for the row, its statistics and the stored info dict,
    // so a failed file write leaves no half-created torrent behind.
    let inserted = conn.transaction::<Torrent, anyhow::Error, _>(|conn| {
        diesel::insert_into(nyaa_torrents::table)
            .values(&new_torrent)
            .execute(conn)?;

        // info_hash is UNIQUE, so this finds our row even with concurrent uploads
        let inserted: Torrent = nyaa_torrents::table
            .filter(nyaa_torrents::info_hash.eq(&new_torrent.info_hash))
            .first(conn)?;

        diesel::insert_into(nyaa_statistics::table)
            .values(&NewStatistic {
                torrent_id: inserted.id,
                seed_count: 0,
                leech_count: 0,
                download_count: 0,
                last_updated: now,
            })
            .execute(conn)?;

        // Write to a temp file and rename, so readers never see a partial file
        let dir: PathBuf = [&cfg.torrent_storage_path, &format!("{}", inserted.id / 1000)]
            .iter().collect();
        std::fs::create_dir_all(&dir)?;
        let file_path = dir.join(format!("{}.torrent.info", inserted.id));
        let tmp_path = dir.join(format!("{}.torrent.info.tmp", inserted.id));
        std::fs::write(&tmp_path, &meta.bencoded_info)?;
        std::fs::rename(&tmp_path, &file_path)?;

        Ok(inserted)
    }).map_err(|e| {
        log::error!("Failed to store upload: {:#}", e);
        actix_web::error::ErrorInternalServerError("Failed to store torrent")
    })?;

    Ok(HttpResponse::Found()
        .insert_header(("Location", format!("/view/{}", inserted.id)))
        .finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::header::TryIntoHeaderValue;

    #[test]
    fn torrent_filename_strips_quotes_separators_and_control_chars() {
        assert_eq!(torrent_filename("My Show [01]"), "My Show [01].torrent");
        assert_eq!(torrent_filename("a\"b/c\\d\r\ne"), "a_b_c_de.torrent");
    }

    #[test]
    fn attachment_header_escapes_and_encodes_name() {
        let value = attachment_header("évil\".torrent").try_into_value().unwrap();
        let value = value.to_str().unwrap();
        assert!(value.starts_with("attachment; filename=\"_vil\\\".torrent\""), "{}", value);
        assert!(value.contains("filename*=UTF-8''%C3%A9vil%22.torrent"), "{}", value);
    }
}
