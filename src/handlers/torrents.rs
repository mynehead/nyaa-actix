use crate::storage::{Kind, Storage};
use actix_multipart::Multipart;
use actix_session::Session;
use actix_web::http::header::{Charset, ContentDisposition, DispositionParam, DispositionType, ExtendedValue};
use actix_web::{web, HttpRequest, HttpResponse, Result};
use diesel::prelude::*;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tera::Tera;

use crate::auth::policy::can_comment;
use crate::auth::{CurrentUser, LoggedIn, Permission};
use crate::config::Config;
use crate::db::schema::{bans, nyaa_comments, nyaa_statistics, nyaa_torrents, users};
use crate::db::DbConnection;
use crate::db::DbPool;
use crate::models::{
    danger_action, edited_flags, torrent_link, user_link, AdminLog, Ban, DangerAction, EditFlags, NewBan, NewStatistic,
    NewTorrent, Torrent, TorrentFlags, User, UserStatus, MAX_BAN_REASON_LEN,
};
use crate::torrent::{parse_torrent, rebuild_torrent};
use crate::utils::context::base_context;
use crate::utils::{client_ip, flash, internal_error, sanitize_string, sanitize_text, unpack_ip};

pub async fn view_torrent(
    CurrentUser(current_user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    storage: web::Data<Storage>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;

    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;

    check_visible(&torrent, &current_user)?;

    let html = render_view(
        &mut conn,
        &tmpl,
        &cfg,
        &storage,
        &torrent,
        current_user.as_ref(),
        "",
        None,
        &flash::take(&session),
    )
    .await?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    pub comment: String,
}

/// Upstream CommentForm limits.
const COMMENT_MIN_LEN: usize = 3;
const COMMENT_MAX_LEN: usize = 2048;

/// Posts a comment from the form on the view page, as upstream's POST /view/<id>.
pub async fn post_comment(
    CurrentUser(current_user): CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    storage: web::Data<Storage>,
    path: web::Path<i32>,
    form: web::Form<CommentForm>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;

    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    check_visible(&torrent, &current_user)?;
    if !can_comment(&torrent, current_user.as_ref()) {
        return Err(actix_web::error::ErrorForbidden("You may not comment on this torrent"));
    }
    let user = current_user.expect("can_comment requires a user");

    let text = sanitize_text(form.comment.trim());
    let len = text.chars().count();
    if !(COMMENT_MIN_LEN..=COMMENT_MAX_LEN).contains(&len) {
        let error =
            format!("Comment must be at least {COMMENT_MIN_LEN} characters long and {COMMENT_MAX_LEN} at most.");
        let html =
            render_view(&mut conn, &tmpl, &cfg, &storage, &torrent, Some(&user), &form.comment, Some(&error), &[])
                .await?;
        return Ok(HttpResponse::BadRequest().content_type("text/html").body(html));
    }

    let count = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            diesel::insert_into(nyaa_comments::table)
                .values(&crate::models::NewComment {
                    torrent_id,
                    user_id: Some(user.id),
                    created_time: chrono::Utc::now().naive_utc(),
                    text,
                })
                .execute(conn)?;
            let count: i64 =
                nyaa_comments::table.filter(nyaa_comments::torrent_id.eq(torrent_id)).count().get_result(conn)?;
            diesel::update(nyaa_torrents::table.find(torrent_id))
                .set(nyaa_torrents::comment_count.eq(count as i32))
                .execute(conn)?;
            Ok(count)
        })
        .map_err(internal_error)?;
    crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), torrent_id);

    Ok(redirect(&format!("/view/{torrent_id}#com-{count}")))
}

#[allow(clippy::too_many_arguments)]
async fn render_view(
    conn: &mut DbConnection,
    tmpl: &Tera,
    cfg: &Config,
    storage: &Storage,
    torrent: &Torrent,
    current_user: Option<&User>,
    comment_text: &str,
    comment_error: Option<&str>,
    flashes: &[flash::Flash],
) -> Result<String> {
    let torrent_id = torrent.id;
    let current_user = current_user.cloned();

    let stats = nyaa_statistics::table
        .find(torrent_id)
        .first::<crate::models::Statistic>(conn)
        .optional()
        .map_err(internal_error)?;

    let comments: Vec<crate::models::Comment> = nyaa_comments::table
        .filter(nyaa_comments::torrent_id.eq(torrent_id))
        .order(nyaa_comments::created_time.asc())
        .load(conn)
        .map_err(internal_error)?;

    // Anonymous uploads only name their uploader to that uploader and moderators
    let can_see_uploader = !torrent.is_anonymous()
        || current_user
            .as_ref()
            .is_some_and(|u| u.can(Permission::ModerateTorrents) || Some(u.id) == torrent.uploader_id);
    let uploader: Option<User> = match torrent.uploader_id {
        Some(uid) if can_see_uploader => User::by_id(conn, uid).map_err(internal_error)?,
        _ => None,
    };

    // Comment authors, for names, level colors and the "(uploader)" tag
    let comments: Vec<serde_json::Value> = comments
        .into_iter()
        .map(|c| {
            let user = c.user_id.and_then(|uid| User::by_id(conn, uid).ok().flatten());
            let avatar_url =
                user.as_ref().map_or_else(|| crate::models::DEFAULT_AVATAR.to_string(), |u| u.avatar_url(cfg));
            serde_json::json!({ "comment": c, "user": user, "avatar_url": avatar_url })
        })
        .collect();

    let main_category = crate::db::schema::nyaa_main_categories::table
        .find(torrent.main_category_id)
        .first::<crate::models::MainCategory>(conn)
        .optional()
        .map_err(internal_error)?;
    let sub_category = crate::models::get_sub_category(conn, torrent.main_category_id, torrent.sub_category_id)
        .map_err(internal_error)?;

    // File list from the stored info dict; missing or unreadable means "not available"
    let info = storage.get(Kind::TorrentInfo, torrent_id).await.unwrap_or_else(|e| {
        log::warn!("Reading info dict of torrent {torrent_id}: {e}");
        None
    });
    let (files, file_count) =
        info.and_then(|info| crate::torrent::file_tree(&info)).map_or((None, 0), |(tree, count)| (Some(tree), count));

    let magnet = torrent.magnet_uri(&torrent.display_name, &cfg.trackers());
    let can_edit = torrent.can_edit(current_user.as_ref());

    // The group the torrent was released under, shown below the submitter
    let group = match torrent.group_id {
        Some(gid) => crate::models::Group::by_id(conn, gid).map_err(internal_error)?,
        None => None,
    };
    // Upstream shows the uploader's IP next to the submitter to administrators only
    let uploader_ip = current_user
        .as_ref()
        .filter(|u| u.can(Permission::SeeIps))
        .and(torrent.uploader_ip.as_deref())
        .and_then(unpack_ip)
        .map(|ip| ip.to_string());

    let mut ctx = base_context(cfg, current_user.as_ref());
    ctx.insert("torrent", &torrent);
    ctx.insert("can_edit", &can_edit);
    ctx.insert("info_hash", &torrent.info_hash_hex());
    ctx.insert("main_category", &main_category);
    ctx.insert("sub_category", &sub_category);
    ctx.insert("stats", &stats);
    ctx.insert("files", &files);
    ctx.insert("file_count", &file_count);
    ctx.insert("max_files_view", &MAX_FILES_VIEW);
    ctx.insert("comments", &comments);
    // Upstream's "Hide comments by default" preference collapses the comments panel
    let hide_comments = match &current_user {
        Some(u) => User::hide_comments(conn, u.id).map_err(internal_error)?,
        None => false,
    };
    ctx.insert("hide_comments", &hide_comments);
    ctx.insert("uploader", &uploader);
    ctx.insert("uploader_ip", &uploader_ip);
    ctx.insert("group", &group);
    ctx.insert("title", group.as_ref().map_or(torrent.display_name.as_str(), |g| g.strip_tag(&torrent.display_name)));
    ctx.insert("magnet", &magnet);
    ctx.insert("can_comment", &can_comment(torrent, current_user.as_ref()));
    ctx.insert("comment_text", comment_text);
    ctx.insert("comment_error", &comment_error);
    ctx.insert("can_report", &crate::handlers::reports::can_report(current_user.as_ref(), cfg));
    ctx.insert("flash_messages", flashes);

    tmpl.render("view.html", &ctx).map_err(internal_error)
}

/// Upstream MAX_FILES_VIEW: longer file lists are not rendered.
const MAX_FILES_VIEW: usize = 1000;

/// Deleted and banned torrents are only visible to moderators.
fn check_visible(torrent: &Torrent, current_user: &Option<User>) -> Result<()> {
    let moderator = current_user.as_ref().is_some_and(|u| u.can(Permission::ModerateTorrents));
    if (torrent.is_deleted() || torrent.is_banned()) && !moderator {
        return Err(actix_web::error::ErrorNotFound("Torrent not found"));
    }
    Ok(())
}

pub async fn download_torrent(
    CurrentUser(current_user): CurrentUser,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    storage: web::Data<Storage>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;

    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    check_visible(&torrent, &current_user)?;

    if torrent.has_torrent == 0 {
        return Err(actix_web::error::ErrorNotFound("Torrent file not available"));
    }

    let bencoded_info = storage
        .get(Kind::TorrentInfo, torrent_id)
        .await
        .map_err(|e| {
            log::error!("Reading info dict of torrent {torrent_id}: {e}");
            actix_web::error::ErrorInternalServerError("Torrent file could not be read")
        })?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent file not found"))?;

    let torrent_data = rebuild_torrent(&torrent, &bencoded_info, &cfg.trackers(), &cfg.site_url);

    Ok(HttpResponse::Ok()
        .content_type("application/x-bittorrent")
        .insert_header(attachment_header(&torrent.torrent_name))
        .body(torrent_data))
}

/// `attachment` with an ASCII `filename` fallback plus the UTF-8 `filename*`,
/// so the uploader-controlled name can't break out of the header.
fn attachment_header(name: &str) -> ContentDisposition {
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '_' }).collect();
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
    let cleaned: String =
        sanitize_string(name).chars().map(|c| if matches!(c, '"' | '/' | '\\') { '_' } else { c }).collect();
    format!("{}.torrent", cleaned.trim())
}

/// Old download URLs, from before they matched upstream's.
pub async fn legacy_download_redirect(path: web::Path<i32>) -> HttpResponse {
    HttpResponse::MovedPermanently().insert_header(("Location", format!("/download/{}.torrent", path))).finish()
}

pub async fn legacy_magnet_redirect(path: web::Path<i32>) -> HttpResponse {
    HttpResponse::MovedPermanently().insert_header(("Location", format!("/view/{}/magnet", path))).finish()
}

pub async fn magnet_redirect(
    CurrentUser(current_user): CurrentUser,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let torrent_id = path.into_inner();
    let mut conn = pool.get().map_err(internal_error)?;
    let torrent = Torrent::by_id(&mut conn, torrent_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    check_visible(&torrent, &current_user)?;
    let magnet = torrent.magnet_uri(&torrent.display_name, &cfg.trackers());
    Ok(HttpResponse::Found().insert_header(("Location", magnet)).finish())
}

pub async fn upload_get(
    CurrentUser(user): CurrentUser,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    let Some(user) = user else {
        return Ok(HttpResponse::Found().insert_header(("Location", "/login")).finish());
    };
    let mut conn = pool.get().map_err(internal_error)?;
    // Trusted users' uploads start out marked trusted, as upstream
    let form = EditForm { is_trusted: user.can(Permission::SetTrustedFlag), ..Default::default() };
    let html = render_upload(&mut conn, &tmpl, &cfg, &user, &form, None, &HashMap::new())?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// The upload page, refilled with `form` and showing `errors` by field
/// (`torrent_file` for problems with the file itself).
fn render_upload(
    conn: &mut DbConnection,
    tmpl: &Tera,
    cfg: &Config,
    user: &User,
    form: &EditForm,
    group_id: Option<i32>,
    errors: &HashMap<&'static str, String>,
) -> Result<String> {
    let categories = crate::models::get_all_categories(conn).map_err(internal_error)?;
    let groups: Vec<crate::models::Group> = crate::models::Group::all(conn)
        .map_err(internal_error)?
        .into_iter()
        .filter(|g| g.can_upload(conn, user.id))
        .collect();
    let mut ctx = base_context(cfg, Some(user));
    ctx.insert("active_page", "upload");
    ctx.insert("categories", &categories);
    ctx.insert("groups", &groups);
    ctx.insert("form", form);
    ctx.insert("group_id", &group_id);
    ctx.insert("errors", errors);
    tmpl.render("upload.html", &ctx).map_err(internal_error)
}

/// nyaa's limit for .torrent files.
pub(crate) const MAX_TORRENT_SIZE: usize = 10 * 1024 * 1024;
/// Per text field (name, information, description, ...).
const MAX_TEXT_FIELD_SIZE: usize = 64 * 1024;
/// Well above what the upload form sends.
const MAX_FIELDS: usize = 32;

/// Reads one multipart field, failing with 413 as soon as it passes `limit`
/// so a huge field is never buffered in memory.
pub(crate) async fn read_field(field: &mut actix_multipart::Field, limit: usize) -> Result<Vec<u8>> {
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
    LoggedIn(user): LoggedIn,
    req: HttpRequest,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    storage: web::Data<Storage>,
    mut payload: Multipart,
) -> Result<HttpResponse> {
    let mut torrent_bytes: Option<Vec<u8>> = None;
    let mut form = EditForm::default();
    let mut group_id: Option<i32> = None;

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
        let text = || String::from_utf8_lossy(&data).into_owned();
        match name.as_str() {
            "torrent_file" => torrent_bytes = Some(data),
            "display_name" => form.display_name = text(),
            "information" => form.information = text(),
            "description" => form.description = text(),
            "category" => form.category = text(),
            "group_id" => group_id = text().parse().ok().filter(|&id| id != 0),
            "is_hidden" => form.is_hidden = !data.is_empty(),
            "is_remake" => form.is_remake = !data.is_empty(),
            "is_anonymous" => form.is_anonymous = !data.is_empty(),
            "is_complete" => form.is_complete = !data.is_empty(),
            "is_trusted" => form.is_trusted = !data.is_empty(),
            _ => {}
        }
    }

    let upload = Upload { torrent_file: torrent_bytes, form, group_id };
    match create_torrent(&pool, &cfg, &storage, &user, client_ip(&req), &upload).await? {
        Ok(torrent) => Ok(HttpResponse::Found().insert_header(("Location", format!("/view/{}", torrent.id))).finish()),
        Err(errors) => {
            let mut conn = pool.get().map_err(internal_error)?;
            let html = render_upload(&mut conn, &tmpl, &cfg, &user, &upload.form, upload.group_id, &errors)?;
            Ok(HttpResponse::BadRequest().content_type("text/html").body(html))
        }
    }
}

/// An upload, from the web form or the API.
pub(crate) struct Upload {
    pub torrent_file: Option<Vec<u8>>,
    pub form: EditForm,
    pub group_id: Option<i32>,
}

/// Errors by upload form field (`torrent_file` for the file itself).
pub(crate) type UploadErrors = HashMap<&'static str, String>;

/// Checks an upload and stores it: the torrent row, its statistics and the info dict.
/// Returns the new torrent, or every problem found by field.
pub(crate) async fn create_torrent(
    pool: &DbPool,
    cfg: &Config,
    storage: &Storage,
    user: &User,
    uploader_ip: Option<Vec<u8>>,
    upload: &Upload,
) -> Result<std::result::Result<Torrent, UploadErrors>> {
    let mut conn = pool.get().map_err(internal_error)?;
    let (form, group_id) = (&upload.form, upload.group_id);
    // The same checks as the edit form, plus the file's own; all are shown at once
    let mut errors = HashMap::new();
    let meta = match upload.torrent_file.as_deref().map(parse_torrent) {
        None => {
            errors.insert("torrent_file", "Please select a torrent file.".to_string());
            None
        }
        Some(Err(e)) => {
            errors.insert("torrent_file", format!("Invalid torrent: {e}"));
            None
        }
        Some(Ok(meta)) => Some(meta),
    };

    // A banned hash can never be uploaded again (the tracker refuses it too). A deleted (but
    // not banned) torrent may be; the new upload replaces it and keeps its id, as upstream.
    let mut replaced_id = None;
    if let Some(meta) = &meta {
        match Torrent::by_info_hash(&mut conn, &meta.info_hash).map_err(internal_error)? {
            Some(t) if t.is_banned() => {
                errors.insert("torrent_file", "This torrent is banned".to_string());
            }
            Some(t) if !t.is_deleted() => {
                errors.insert("torrent_file", format!("This torrent already exists (#{})", t.id));
            }
            Some(t) => replaced_id = Some(t.id),
            None => {}
        }
    }

    // A blank display name means the torrent's own name, which must fit the same limits
    let final_name = match (form.display_name.trim(), &meta) {
        ("", Some(meta)) => sanitize_string(&meta.display_name),
        (name, _) => sanitize_string(name),
    };
    let checked = EditForm { display_name: final_name.clone(), ..form.clone() };
    let categories = match checked.validate(&mut conn) {
        Ok(ids) => Some(ids),
        Err(field_errors) => {
            errors.extend(field_errors);
            None
        }
    };
    let (Some(meta), Some((main_cat, sub_cat)), true) = (meta, categories, errors.is_empty()) else {
        return Ok(Err(errors));
    };

    // Only groups the user may upload for; anything else is a personal upload
    let resolved_group = match group_id {
        Some(gid) => crate::models::Group::by_id(&mut conn, gid)
            .map_err(internal_error)?
            .filter(|g| g.can_upload(&mut conn, user.id))
            .map(|g| g.id),
        None => None,
    };

    let mut flags = TorrentFlags::empty();
    flags.set(TorrentFlags::HIDDEN, form.is_hidden);
    flags.set(TorrentFlags::REMAKE, form.is_remake);
    flags.set(TorrentFlags::ANONYMOUS, form.is_anonymous);
    flags.set(TorrentFlags::COMPLETE, form.is_complete);
    flags.set(TorrentFlags::TRUSTED, form.is_trusted && user.can(Permission::SetTrustedFlag));
    let flags = flags.bits();

    let now = chrono::Utc::now().naive_utc();

    let new_torrent = NewTorrent {
        id: replaced_id,
        info_hash: meta.info_hash.clone(),
        display_name: final_name,
        torrent_name: torrent_filename(&meta.display_name),
        information: sanitize_string(form.information.trim()),
        description: sanitize_text(form.description.trim()),
        filesize: meta.filesize,
        encoding: meta.encoding.clone(),
        flags,
        uploader_id: Some(user.id),
        uploader_ip,
        has_torrent: 1,
        comment_count: 0,
        created_time: now,
        updated_time: now,
        main_category_id: main_cat,
        sub_category_id: sub_cat,
        group_id: resolved_group,
    };

    // One transaction for the row and its statistics. The info dict is stored after the
    // commit (S3 writes can't sit inside a database transaction); if that fails the rows
    // are removed again, so no torrent is left without its file.
    let inserted = conn
        .transaction::<Torrent, anyhow::Error, _>(|conn| {
            if let Some(old_id) = replaced_id {
                diesel::delete(nyaa_comments::table.filter(nyaa_comments::torrent_id.eq(old_id))).execute(conn)?;
                diesel::delete(nyaa_statistics::table.find(old_id)).execute(conn)?;
                diesel::delete(nyaa_torrents::table.find(old_id)).execute(conn)?;
            }
            diesel::insert_into(nyaa_torrents::table).values(&new_torrent).execute(conn)?;

            // info_hash is UNIQUE, so this finds our row even with concurrent uploads
            let inserted: Torrent =
                nyaa_torrents::table.filter(nyaa_torrents::info_hash.eq(&new_torrent.info_hash)).first(conn)?;

            diesel::insert_into(nyaa_statistics::table)
                .values(&NewStatistic {
                    torrent_id: inserted.id,
                    seed_count: 0,
                    leech_count: 0,
                    download_count: 0,
                    last_updated: now,
                })
                .execute(conn)?;

            Ok(inserted)
        })
        .map_err(|e| {
            log::error!("Failed to store upload: {:#}", e);
            actix_web::error::ErrorInternalServerError("Failed to store torrent")
        })?;

    // Don't hold a pooled connection while waiting on the store
    drop(conn);
    let stored = storage.put(Kind::TorrentInfo, inserted.id, meta.bencoded_info).await;
    let mut conn = pool.get().map_err(internal_error)?;
    if let Err(e) = stored {
        log::error!("Failed to store info dict of torrent {}: {}", inserted.id, e);
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            diesel::delete(nyaa_statistics::table.find(inserted.id)).execute(conn)?;
            diesel::delete(nyaa_torrents::table.find(inserted.id)).execute(conn)
        })
        .map_err(|e| log::error!("Removing torrent {} after the failed write: {}", inserted.id, e))
        .ok();
        return Err(actix_web::error::ErrorInternalServerError("Failed to store torrent"));
    }
    crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), inserted.id);
    crate::tracker::torrent_changed(cfg.tracker.as_ref(), inserted.id);
    Ok(Ok(inserted))
}

/// The edit page's two forms post here: "Save Changes" with the fields, or one
/// Danger Zone button (upstream `EditForm` and `DeleteForm`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EditForm {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub information: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, deserialize_with = "checkbox")]
    pub is_hidden: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub is_remake: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub is_anonymous: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub is_complete: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub is_trusted: bool,
    #[serde(default, deserialize_with = "checkbox")]
    pub is_comment_locked: bool,
    /// The group select ("0" for none); absent when the page showed no select,
    /// which keeps the torrent's group.
    #[serde(default, skip_serializing)]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub submit: Option<String>,
    #[serde(default, skip_serializing)]
    pub delete: Option<String>,
    #[serde(default, skip_serializing)]
    pub ban: Option<String>,
    #[serde(default, skip_serializing)]
    pub undelete: Option<String>,
    #[serde(default, skip_serializing)]
    pub unban: Option<String>,
    /// The uploader ban buttons (upstream `BanForm` on the edit page), with their reason.
    #[serde(default, skip_serializing)]
    pub ban_user: Option<String>,
    #[serde(default, skip_serializing)]
    pub ban_userip: Option<String>,
    #[serde(default, skip_serializing)]
    pub reason: String,
}

/// A checked box sends its value; an unchecked one sends nothing.
fn checkbox<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<bool, D::Error> {
    Ok(!String::deserialize(d)?.is_empty())
}

impl EditForm {
    fn from_torrent(t: &Torrent) -> Self {
        EditForm {
            display_name: t.display_name.clone(),
            category: format!("{}_{}", t.main_category_id, t.sub_category_id),
            information: t.information.clone(),
            description: t.description.clone(),
            is_hidden: t.is_hidden(),
            is_remake: t.is_remake(),
            is_anonymous: t.is_anonymous(),
            is_complete: t.is_complete(),
            is_trusted: t.is_trusted(),
            is_comment_locked: t.is_comment_locked(),
            ..Default::default()
        }
    }

    fn flags(&self) -> EditFlags {
        EditFlags {
            hidden: self.is_hidden,
            remake: self.is_remake,
            complete: self.is_complete,
            anonymous: self.is_anonymous,
            trusted: self.is_trusted,
            comment_locked: self.is_comment_locked,
        }
    }

    /// The Danger Zone button that was pressed, if any.
    fn danger_action(&self) -> Option<DangerAction> {
        if self.delete.is_some() {
            Some(DangerAction::Delete)
        } else if self.ban.is_some() {
            Some(DangerAction::Ban)
        } else if self.undelete.is_some() {
            Some(DangerAction::Undelete)
        } else if self.unban.is_some() {
            Some(DangerAction::Unban)
        }
        // Banning the uploader also bans the torrent, as upstream
        else if self.bans_uploader() {
            Some(DangerAction::Ban)
        } else {
            None
        }
    }

    fn bans_uploader(&self) -> bool {
        self.ban_user.is_some() || self.ban_userip.is_some()
    }

    /// Upstream's `EditForm` validators. Returns the category ids, or errors by field.
    fn validate(&self, conn: &mut DbConnection) -> std::result::Result<(i32, i32), HashMap<&'static str, String>> {
        let mut errors = HashMap::new();
        let name_len = self.display_name.trim().chars().count();
        if !(3..=255).contains(&name_len) {
            errors.insert(
                "display_name",
                "Torrent display name must be at least 3 characters long and 255 at most.".to_string(),
            );
        }
        if self.information.trim().chars().count() > 255 {
            errors.insert("information", "Information must be at most 255 characters long.".to_string());
        }
        if self.description.trim().chars().count() > MAX_DESCRIPTION_LEN {
            errors
                .insert("description", format!("Description must be at most {} characters long.", MAX_DESCRIPTION_LEN));
        }
        let category = parse_category(&self.category);
        match category {
            None => {
                errors.insert("category", "Please select a category".to_string());
            }
            // "N_0" is a main category, which can't be picked
            Some((main, sub)) => {
                let exists =
                    sub != 0 && crate::models::get_sub_category(conn, main, sub).map(|c| c.is_some()).unwrap_or(false);
                if !exists {
                    errors.insert("category", "Please select a proper category".to_string());
                }
            }
        }
        match category {
            Some(ids) if errors.is_empty() => Ok(ids),
            _ => Err(errors),
        }
    }
}

/// Upstream's description limit (10 KiB of characters).
const MAX_DESCRIPTION_LEN: usize = 10 * 1024;

/// "main_sub" from the category select.
fn parse_category(value: &str) -> Option<(i32, i32)> {
    let (main, sub) = value.split_once('_')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(main) || !digits(sub) {
        return None;
    }
    Some((main.parse().ok()?, sub.parse().ok()?))
}

/// The torrent behind an edit request, or 404/403 as upstream: deleted torrents only
/// exist for moderators, and only owners and moderators may edit.
fn editable_torrent(conn: &mut DbConnection, torrent_id: i32, editor: Option<&User>) -> Result<Torrent> {
    let torrent = Torrent::by_id(conn, torrent_id)
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("Torrent not found"))?;
    let is_moderator = editor.is_some_and(|u| u.can(Permission::ModerateTorrents));
    if (torrent.is_deleted() || torrent.is_banned()) && !is_moderator {
        return Err(actix_web::error::ErrorNotFound("Torrent not found"));
    }
    if !torrent.can_edit(editor) {
        return Err(actix_web::error::ErrorForbidden("You may not edit this torrent"));
    }
    Ok(torrent)
}

/// The group a saved edit leaves the torrent under. Keeping the current group or
/// clearing it is always allowed; moving to another group needs upload rights in it,
/// as on the upload page, unless the editor is a moderator.
fn edited_group(
    conn: &mut DbConnection,
    form: &EditForm,
    torrent: &Torrent,
    editor: &User,
) -> QueryResult<std::result::Result<Option<i32>, String>> {
    let Some(value) = form.group_id.as_deref() else {
        return Ok(Ok(torrent.group_id));
    };
    let gid = match value.trim().parse::<i32>() {
        Ok(0) => return Ok(Ok(None)),
        Ok(gid) => gid,
        Err(_) => return Ok(Err("Please select a proper group".to_string())),
    };
    if Some(gid) == torrent.group_id {
        return Ok(Ok(Some(gid)));
    }
    let allowed = match crate::models::Group::by_id(conn, gid)? {
        Some(g) => editor.can(Permission::ModerateTorrents) || g.can_upload(conn, editor.id),
        None => false,
    };
    Ok(if allowed { Ok(Some(gid)) } else { Err("You may not release under this group".to_string()) })
}

#[allow(clippy::too_many_arguments)]
fn render_edit(
    conn: &mut DbConnection,
    tmpl: &Tera,
    cfg: &Config,
    editor: &User,
    torrent: &Torrent,
    form: &EditForm,
    errors: &HashMap<&'static str, String>,
    flashes: &[flash::Flash],
) -> Result<String> {
    let categories = crate::models::get_all_categories(conn).map_err(internal_error)?;
    // The "(by user)" note when someone else's torrent is edited
    let uploader = match torrent.uploader_id {
        Some(uid) if uid != editor.id => User::by_id(conn, uid).map_err(internal_error)?,
        _ => None,
    };
    // Groups the editor may release under (all of them for moderators), plus the
    // torrent's current group so keeping it is always possible
    let is_moderator = editor.can(Permission::ModerateTorrents);
    let groups: Vec<crate::models::Group> = crate::models::Group::all(conn)
        .map_err(internal_error)?
        .into_iter()
        .filter(|g| is_moderator || Some(g.id) == torrent.group_id || g.can_upload(conn, editor.id))
        .collect();
    let group_id = match form.group_id.as_deref() {
        Some(value) => value.parse::<i32>().ok().filter(|&id| id != 0),
        None => torrent.group_id,
    };
    let mut ctx = base_context(cfg, Some(editor));
    ctx.insert("torrent", torrent);
    ctx.insert("form", form);
    ctx.insert("errors", errors);
    ctx.insert("categories", &categories);
    ctx.insert("groups", &groups);
    ctx.insert("group_id", &group_id);
    ctx.insert("uploader", &uploader);
    ctx.insert("is_deleted", &torrent.is_deleted());
    ctx.insert("is_banned", &torrent.is_banned());
    ctx.insert("flash_messages", flashes);
    let target = UploaderBanTarget::load(conn, torrent).map_err(actix_web::error::ErrorInternalServerError)?;
    if target.can_be_banned_by(editor) {
        ctx.insert("ban_form", &true);
        ctx.insert("ban_uploader", &target.uploader);
        ctx.insert("ip_banned", &target.ip_banned());
    }
    tmpl.render("edit.html", &ctx).map_err(actix_web::error::ErrorInternalServerError)
}

pub async fn edit_torrent_get(
    CurrentUser(editor): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let torrent = editable_torrent(&mut conn, path.into_inner(), editor.as_ref())?;
    let editor = editor.expect("editable_torrent requires a user");
    let html = render_edit(
        &mut conn,
        &tmpl,
        &cfg,
        &editor,
        &torrent,
        &EditForm::from_torrent(&torrent),
        &HashMap::new(),
        &flash::take(&session),
    )?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn edit_torrent_post(
    CurrentUser(editor): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<i32>,
    form: web::Form<EditForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let torrent = editable_torrent(&mut conn, path.into_inner(), editor.as_ref())?;
    let editor = editor.expect("editable_torrent requires a user");
    let form = form.into_inner();
    let view_url = format!("/view/{}", torrent.id);

    if form.submit.is_some() {
        let validated = form.validate(&mut conn);
        let group = edited_group(&mut conn, &form, &torrent, &editor).map_err(internal_error)?;
        let ((main_cat, sub_cat), group_id) = match (validated, group) {
            (Ok(ids), Ok(group_id)) => (ids, group_id),
            (validated, group) => {
                let mut errors = validated.err().unwrap_or_default();
                if let Err(e) = group {
                    errors.insert("group_id", e);
                }
                let html = render_edit(&mut conn, &tmpl, &cfg, &editor, &torrent, &form, &errors, &[])?;
                return Ok(HttpResponse::BadRequest().content_type("text/html").body(html));
            }
        };
        // Only uploads with an uploader can be anonymous (upstream hides the box otherwise)
        let mut edit = form.flags();
        edit.anonymous &= torrent.uploader_id.is_some();
        let new_flags = edited_flags(torrent.flags, &edit, &editor);
        let lock_changed = (new_flags ^ torrent.flags) & TorrentFlags::COMMENT_LOCKED.bits() != 0;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            diesel::update(nyaa_torrents::table.find(torrent.id))
                .set((
                    nyaa_torrents::display_name.eq(sanitize_string(form.display_name.trim())),
                    nyaa_torrents::information.eq(sanitize_string(form.information.trim())),
                    nyaa_torrents::description.eq(sanitize_text(form.description.trim())),
                    nyaa_torrents::main_category_id.eq(main_cat),
                    nyaa_torrents::sub_category_id.eq(sub_cat),
                    nyaa_torrents::group_id.eq(group_id),
                    nyaa_torrents::flags.eq(new_flags),
                    nyaa_torrents::updated_time.eq(chrono::Utc::now().naive_utc()),
                ))
                .execute(conn)?;
            // Only moderators can change the lock (edited_flags), and upstream logs each change
            if lock_changed {
                let locked = new_flags & TorrentFlags::COMMENT_LOCKED.bits() != 0;
                AdminLog::add(
                    conn,
                    editor.id,
                    &format!(
                        "Torrent {} marked as {}",
                        torrent_link(torrent.id),
                        if locked { "comments locked" } else { "comments unlocked" }
                    ),
                )?;
            }
            Ok(())
        })
        .map_err(internal_error)?;
        crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), torrent.id);
        return Ok(redirect(&view_url));
    }

    let edit_url = format!("{}/edit", view_url);
    let torrent_action = form.danger_action().and_then(|a| danger_action(torrent.flags, a, &editor));
    let uploader_ban = if form.bans_uploader() {
        let target =
            UploaderBanTarget::load(&mut conn, &torrent).map_err(actix_web::error::ErrorInternalServerError)?;
        if !target.can_be_banned_by(&editor) {
            return Err(actix_web::error::ErrorForbidden("You may not ban this uploader"));
        }
        match target.plan(&form, &torrent, &cfg) {
            Ok(plan) => Some(plan),
            Err(message) => {
                flash::push(&session, "danger", "", message);
                return Ok(redirect(&edit_url));
            }
        }
    } else {
        None
    };
    if torrent_action.is_none() && uploader_ban.is_none() {
        // A button that doesn't apply here (upstream flashes an error and goes back)
        return Ok(redirect(&edit_url));
    }
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        if let Some((flags, action)) = torrent_action {
            log::info!("Torrent #{} {} by {}", torrent.id, action, editor.username);
            diesel::update(nyaa_torrents::table.find(torrent.id)).set(nyaa_torrents::flags.eq(flags)).execute(conn)?;
            // Upstream also drops banned torrents from the tracker, so their peers are gone
            if flags & TorrentFlags::BANNED.bits() != 0 {
                diesel::update(nyaa_statistics::table.find(torrent.id))
                    .set((nyaa_statistics::seed_count.eq(0), nyaa_statistics::leech_count.eq(0)))
                    .execute(conn)?;
            }
            // Upstream logs moderator actions on other people's torrents
            if editor.can(Permission::ModerateTorrents) && torrent.uploader_id != Some(editor.id) {
                AdminLog::add(conn, editor.id, &format!("Torrent {} has been {}", torrent_link(torrent.id), action))?;
            }
        }
        if let Some(plan) = &uploader_ban {
            plan.apply(conn, &editor, torrent.id)?;
        }
        Ok(())
    })
    .map_err(actix_web::error::ErrorInternalServerError)?;
    // Only the torrent page shows flashes; owners deleting their own torrent go home
    if let Some((_, action)) = torrent_action.filter(|_| editor.can(Permission::ModerateTorrents)) {
        flash::push(&session, "success", "", &format!("Torrent has been successfully {}.", action));
    }
    if uploader_ban.is_some() {
        flash::push(&session, "success", "", "Uploader has been successfully banned.");
    }
    crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), torrent.id);
    crate::tracker::torrent_changed(cfg.tracker.as_ref(), torrent.id);

    // Moderators go back to the torrent; owners deleting their own go home
    Ok(redirect(if editor.can(Permission::ModerateTorrents) { &view_url } else { "/" }))
}

/// A torrent's uploader as the edit page's ban buttons see them (upstream `_delete_torrent`).
struct UploaderBanTarget {
    uploader: Option<User>,
    uploader_ip: Option<Vec<u8>>,
    /// Whether the torrent's upload IP is already banned (or unknown, so there is nothing to ban).
    torrent_ip_banned: bool,
    /// Whether the uploader's last login IP is already banned (or there is no uploader or IP).
    user_ip_banned: bool,
}

/// The bans one uploader-ban button press will add.
struct UploaderBanPlan {
    user_id: Option<i32>,
    ips: Vec<Vec<u8>>,
    reason: String,
    /// Upstream's "[name](/user/name) IP(...)" or "Anonymous IP(...)".
    uploader_str: String,
}

impl UploaderBanTarget {
    fn load(conn: &mut DbConnection, torrent: &Torrent) -> QueryResult<Self> {
        let uploader = match torrent.uploader_id {
            Some(uid) => User::by_id(conn, uid)?,
            None => None,
        };
        let ip_banned = |conn: &mut DbConnection, ip: Option<&[u8]>| match ip {
            Some(ip) => Ban::ip_banned(conn, ip),
            None => Ok(true),
        };
        let torrent_ip_banned = ip_banned(conn, torrent.uploader_ip.as_deref())?;
        let user_ip_banned = ip_banned(conn, uploader.as_ref().and_then(|u| u.last_login_ip.as_deref()))?;
        Ok(UploaderBanTarget { uploader, uploader_ip: torrent.uploader_ip.clone(), torrent_ip_banned, user_ip_banned })
    }

    /// Moderators may ban uploaders ranked below them, and anonymous (account-less) uploads.
    fn can_be_banned_by(&self, editor: &User) -> bool {
        editor.can(Permission::BanUsers) && self.uploader.as_ref().is_none_or(|u| u.level() < editor.level())
    }

    fn ip_banned(&self) -> bool {
        self.torrent_ip_banned && self.user_ip_banned
    }

    /// Checks the button against the uploader's state and works out the bans, or the
    /// message to flash when it doesn't apply.
    fn plan(
        &self,
        form: &EditForm,
        torrent: &Torrent,
        cfg: &Config,
    ) -> std::result::Result<UploaderBanPlan, &'static str> {
        let ban_ip = form.ban_userip.is_some();
        let reason = form.reason.trim();
        if reason.is_empty() {
            return Err("Please specify a ban reason.");
        }
        if reason.chars().count() > MAX_BAN_REASON_LEN {
            return Err("Reason must be at most 1024 characters long.");
        }
        let user_banned = self.uploader.as_ref().map(|u| u.is_banned()).unwrap_or(true);
        if (!ban_ip && user_banned) || (ban_ip && self.ip_banned()) {
            return Err("That action doesn't apply to this uploader.");
        }
        // Upstream bans the uploader's login IP and, when different, the upload IP
        let mut ips = Vec::new();
        if ban_ip {
            if !self.user_ip_banned {
                ips.extend(self.uploader.as_ref().and_then(|u| u.last_login_ip.clone()));
            }
            if !self.torrent_ip_banned {
                ips.extend(self.uploader_ip.clone());
            }
            ips.dedup();
            if ips.iter().filter_map(|ip| unpack_ip(ip)).any(|ip| ip.is_loopback()) {
                return Err("The uploader's IP is a loopback address, which would ban everyone behind the proxy.");
            }
        }
        let flavor = if cfg.site_flavor == "sukebei" { "Sukebei" } else { "Nyaa" };
        let url = format!("{}/view/{}", cfg.site_url.trim_end_matches('/'), torrent.id);
        let mut uploader_str = match &self.uploader {
            Some(u) => user_link(&u.username),
            None => "Anonymous".to_string(),
        };
        for ip in ips.iter().filter_map(|ip| unpack_ip(ip)) {
            uploader_str.push_str(&format!(" IP({})", ip));
        }
        Ok(UploaderBanPlan {
            user_id: self.uploader.as_ref().map(|u| u.id),
            ips,
            reason: format!("[{}#{}]({}) {}", flavor, torrent.id, url, sanitize_text(reason)),
            uploader_str,
        })
    }
}

impl UploaderBanPlan {
    /// Bans the uploader account and IPs and logs it; runs inside the edit transaction.
    fn apply(&self, conn: &mut DbConnection, editor: &User, torrent_id: i32) -> QueryResult<()> {
        if let Some(uid) = self.user_id {
            diesel::update(users::table.find(uid)).set(users::status.eq(UserStatus::Banned as i32)).execute(conn)?;
        }
        // One ban per IP; a user-only ban when there are none
        let ips: Vec<Option<Vec<u8>>> =
            if self.ips.is_empty() { vec![None] } else { self.ips.iter().cloned().map(Some).collect() };
        for user_ip in ips {
            diesel::insert_into(bans::table)
                .values(NewBan {
                    created_time: chrono::Utc::now().naive_utc(),
                    admin_id: editor.id,
                    user_id: self.user_id,
                    user_ip,
                    reason: self.reason.clone(),
                })
                .execute(conn)?;
        }
        AdminLog::add(
            conn,
            editor.id,
            &format!("Uploader {} of torrent {} has been banned.", self.uploader_str, torrent_link(torrent_id)),
        )
    }
}

fn redirect(location: &str) -> HttpResponse {
    HttpResponse::Found().insert_header(("Location", location)).finish()
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

    #[test]
    fn parses_categories() {
        assert_eq!(parse_category("1_2"), Some((1, 2)));
        assert_eq!(parse_category("12_0"), Some((12, 0)));
        for bad in ["", "1", "1_", "_2", "1_2_3", "a_1", "-1_2", "1_+2"] {
            assert_eq!(parse_category(bad), None, "{bad}");
        }
    }

    mod edit_page {
        use super::super::*;
        use actix_session::{storage::CookieSessionStore, SessionMiddleware};
        use actix_web::{
            cookie::{Cookie, Key},
            http::StatusCode,
            test, App,
        };
        use diesel::r2d2::Pool;

        fn pool() -> DbPool {
            // One connection, so every request sees the same in-memory database
            let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
            let mut conn = pool.get().unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            diesel::sql_query(
                "INSERT INTO users (id, username, password_hash, status, level) VALUES \
                               (1, 'owner', 'x', 1, 0), (2, 'other', 'x', 1, 1), (3, 'mod', 'x', 1, 2)",
            )
            .execute(&mut conn)
            .unwrap();
            diesel::sql_query(format!(
                "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, information, description, \
                 flags, uploader_id, main_category_id, sub_category_id) \
                 VALUES (5, X'{}', 'Old name', 'old.torrent', '', 'old', {}, 1, 1, 2)",
                "ab".repeat(20),
                TorrentFlags::TRUSTED.bits()
            ))
            .execute(&mut conn)
            .unwrap();
            diesel::sql_query(
                "INSERT INTO nyaa_statistics (torrent_id, seed_count, leech_count, download_count) \
                               VALUES (5, 4, 3, 9)",
            )
            .execute(&mut conn)
            .unwrap();
            pool
        }

        fn config() -> Config {
            let storage = std::env::temp_dir().join(format!("nyaa-edit-test-{}", std::process::id()));
            Config {
                database_url: String::new(),
                secret_key: String::new(),
                site_name: "Nyaa".into(),
                site_flavor: "nyaa".into(),
                results_per_page: 75,
                max_pages: 0,
                torrent_storage_path: storage.to_string_lossy().into_owned(),
                avatar_storage_path: String::new(),
                enable_gravatar: false,
                maintenance_mode: false,
                site_url: String::new(),
                tracker_urls: vec![],
                trusted_proxies: vec![],
                meili: None,
                tracker: None,
                ratelimit_account_age: 0,
                trusted: Default::default(),
                tickets: Default::default(),
                mfa: Default::default(),
            }
        }

        fn storage() -> Storage {
            let dir = config().torrent_storage_path;
            Storage::local(&dir, &dir).unwrap()
        }

        use crate::middleware::auth::test_support::login;

        /// The edit routes plus a login shortcut; returns the app and a session cookie for `user`.
        macro_rules! app {
            ($pool:expr, $user:expr) => {
                app!($pool, $user, storage())
            };
            ($pool:expr, $user:expr, $storage:expr) => {{
                let mut tera = Tera::new("templates/**/*").unwrap();
                crate::utils::tera_filters::register(&mut tera);
                let app = test::init_service(
                    App::new()
                        .app_data(web::Data::new(config()))
                        .app_data(web::Data::new($pool.clone()))
                        .app_data(web::Data::new($storage))
                        .app_data(web::Data::new(tera))
                        .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                        .route("/login/{id}", web::get().to(login))
                        .route("/view/{id}", web::get().to(view_torrent))
                        .route("/view/{id}", web::post().to(post_comment))
                        .route("/view/{id}/edit", web::get().to(edit_torrent_get))
                        .route("/view/{id}/edit", web::post().to(edit_torrent_post))
                        .route("/upload", web::post().to(upload_post)),
                )
                .await;
                let cookie: Option<Cookie<'static>> = match $user {
                    Some(id) => {
                        let res = test::call_service(
                            &app,
                            test::TestRequest::get().uri(&format!("/login/{}", id)).to_request(),
                        )
                        .await;
                        res.response().cookies().next().map(|c| c.into_owned())
                    }
                    None => None,
                };
                (app, cookie)
            }};
        }

        fn get(uri: &str, cookie: &Option<Cookie<'static>>) -> test::TestRequest {
            let mut req = test::TestRequest::get().uri(uri);
            if let Some(c) = cookie {
                req = req.cookie(c.clone());
            }
            req
        }

        fn post(uri: &str, cookie: &Option<Cookie<'static>>, form: &[(&str, &str)]) -> test::TestRequest {
            let mut req = test::TestRequest::post().uri(uri).set_form(form);
            if let Some(c) = cookie {
                req = req.cookie(c.clone());
            }
            req
        }

        fn torrent(pool: &DbPool) -> Torrent {
            Torrent::by_id(&mut pool.get().unwrap(), 5).unwrap().unwrap()
        }

        fn location(res: &actix_web::dev::ServiceResponse) -> &str {
            res.headers().get("Location").unwrap().to_str().unwrap()
        }

        #[actix_web::test]
        async fn pencil_and_edit_page_only_for_owner_and_moderators() {
            let pool = pool();
            for (user, status, pencil) in [
                (None, StatusCode::FORBIDDEN, false),
                (Some(2), StatusCode::FORBIDDEN, false),
                (Some(1), StatusCode::OK, true),
                (Some(3), StatusCode::OK, true),
            ] {
                let (app, cookie) = app!(pool, user);
                let view = test::call_and_read_body(&app, get("/view/5", &cookie).to_request()).await;
                let view = String::from_utf8(view.to_vec()).unwrap();
                assert_eq!(view.contains("href=\"/view/5/edit\""), pencil, "{user:?}");
                let res = test::call_service(&app, get("/view/5/edit", &cookie).to_request()).await;
                assert_eq!(res.status(), status, "{user:?}");
            }

            let (app, cookie) = app!(pool, Some(3));
            let page = test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await;
            let page = String::from_utf8(page.to_vec()).unwrap();
            assert!(page.contains("value=\"Old name\""), "{page}");
            assert!(page.contains("<option value=\"1_2\" selected>"), "{page}");
            assert!(page.contains("(by <a href=\"/user/owner\">owner</a>)"), "{page}");
            assert!(page.contains("name=\"is_comment_locked\""), "moderators see the lock");
            assert!(page.contains("name=\"ban\""));
        }

        #[actix_web::test]
        async fn group_row_and_admin_only_uploader_ip() {
            let pool = pool();
            {
                let mut conn = pool.get().unwrap();
                diesel::sql_query(
                    "INSERT INTO users (id, username, password_hash, status, level) VALUES (4, 'admin', 'x', 1, 3)",
                )
                .execute(&mut conn)
                .unwrap();
                diesel::sql_query(
                    "INSERT INTO groups (id, name, tag, slug, created_time, owner_id) \
                                   VALUES (1, 'Cyan', 'Cyan', 'cyan', CURRENT_TIMESTAMP, 1)",
                )
                .execute(&mut conn)
                .unwrap();
            }
            let (app, cookie) = app!(pool, Some(4));
            let view =
                String::from_utf8(test::call_and_read_body(&app, get("/view/5", &cookie).to_request()).await.to_vec())
                    .unwrap();
            assert!(!view.contains("Group:"), "no group row without a group");

            diesel::sql_query(format!(
                "UPDATE nyaa_torrents SET group_id = 1, uploader_ip = X'{}' WHERE id = 5",
                hex::encode(crate::utils::pack_ip("127.0.0.1".parse().unwrap()))
            ))
            .execute(&mut pool.get().unwrap())
            .unwrap();
            for (user, sees_ip) in [(None, false), (Some(1), false), (Some(3), false), (Some(4), true)] {
                let (app, cookie) = app!(pool, user);
                let view = String::from_utf8(
                    test::call_and_read_body(&app, get("/view/5", &cookie).to_request()).await.to_vec(),
                )
                .unwrap();
                assert!(view.contains("<div class=\"col-md-1\">Group:</div>"), "{user:?}");
                assert!(view.contains("<a href=\"/group/cyan\">[Cyan] Cyan</a>"), "{view}");
                assert!(
                    view.contains("<a href=\"/group/cyan\" class=\"group-tag\" title=\"Cyan\">[Cyan]</a> Old name"),
                    "{view}"
                );
                assert_eq!(view.contains("(127.0.0.1)"), sees_ip, "{user:?}");
            }
        }

        #[actix_web::test]
        async fn group_can_be_set_changed_and_cleared_on_edit() {
            let pool = pool();
            diesel::sql_query(
                "INSERT INTO groups (id, name, tag, slug, created_time, owner_id) VALUES \
                 (1, 'Cyan', 'Cyan', 'cyan', CURRENT_TIMESTAMP, 1), \
                 (2, 'Magenta', 'Mag', 'magenta', CURRENT_TIMESTAMP, 2)",
            )
            .execute(&mut pool.get().unwrap())
            .unwrap();
            let save = |group: Option<&'static str>| {
                let mut form = vec![("display_name", "Old name"), ("category", "1_2"), ("submit", "Save Changes")];
                if let Some(g) = group {
                    form.push(("group_id", g));
                }
                form
            };

            // The owner only gets the groups they may upload for
            let (app, cookie) = app!(pool, Some(1));
            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(page.contains("<option value=\"0\">None (personal upload)</option>"), "{page}");
            assert!(page.contains("<option value=\"1\">[Cyan] Cyan</option>"), "{page}");
            assert!(!page.contains("[Mag] Magenta"), "{page}");

            let res = test::call_service(&app, post("/view/5/edit", &cookie, &save(Some("1"))).to_request()).await;
            assert_eq!(res.status(), StatusCode::FOUND);
            assert_eq!(torrent(&pool).group_id, Some(1));
            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(page.contains("<option value=\"1\" selected>[Cyan] Cyan</option>"), "{page}");

            // Someone else's group is refused and nothing is saved
            let res = test::call_service(&app, post("/view/5/edit", &cookie, &save(Some("2"))).to_request()).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
            assert!(page.contains("You may not release under this group"), "{page}");
            assert_eq!(torrent(&pool).group_id, Some(1));

            // No select sent keeps the group; "0" clears it
            test::call_service(&app, post("/view/5/edit", &cookie, &save(None)).to_request()).await;
            assert_eq!(torrent(&pool).group_id, Some(1));
            test::call_service(&app, post("/view/5/edit", &cookie, &save(Some("0"))).to_request()).await;
            assert_eq!(torrent(&pool).group_id, None);

            // Moderators may pick any group
            let (app, cookie) = app!(pool, Some(3));
            let res = test::call_service(&app, post("/view/5/edit", &cookie, &save(Some("2"))).to_request()).await;
            assert_eq!(res.status(), StatusCode::FOUND);
            assert_eq!(torrent(&pool).group_id, Some(2));

            // The owner keeps a group they aren't in when saving other changes
            let (app, cookie) = app!(pool, Some(1));
            let res = test::call_service(&app, post("/view/5/edit", &cookie, &save(Some("2"))).to_request()).await;
            assert_eq!(res.status(), StatusCode::FOUND);
            assert_eq!(torrent(&pool).group_id, Some(2));
        }

        #[actix_web::test]
        async fn owner_edits_fields_but_not_trusted() {
            let pool = pool();
            let (app, cookie) = app!(pool, Some(1));
            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(!page.contains("name=\"is_trusted\"") && !page.contains("name=\"ban\""), "{page}");

            let res = test::call_service(
                &app,
                post(
                    "/view/5/edit",
                    &cookie,
                    &[
                        ("display_name", "  New name "),
                        ("category", "2_1"),
                        ("information", "#chan@irc.example"),
                        ("description", "line 1\r\nline 2"),
                        ("is_remake", "y"),
                        ("submit", "Save Changes"),
                    ],
                )
                .to_request(),
            )
            .await;
            assert_eq!(res.status(), StatusCode::FOUND);
            assert_eq!(location(&res), "/view/5");
            let t = torrent(&pool);
            assert_eq!((t.display_name.as_str(), t.main_category_id, t.sub_category_id), ("New name", 2, 1));
            assert_eq!((t.information.as_str(), t.description.as_str()), ("#chan@irc.example", "line 1\nline 2"));
            assert_eq!(t.flags, (TorrentFlags::TRUSTED | TorrentFlags::REMAKE).bits());
        }

        #[actix_web::test]
        async fn invalid_edit_rerenders_with_errors() {
            let pool = pool();
            let (app, cookie) = app!(pool, Some(1));
            let res = test::call_service(
                &app,
                post(
                    "/view/5/edit",
                    &cookie,
                    &[("display_name", "ab"), ("category", "1_0"), ("submit", "Save Changes")],
                )
                .to_request(),
            )
            .await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
            assert!(page.contains("must be at least 3 characters"), "{page}");
            assert!(page.contains("Please select a proper category"), "{page}");
            assert!(page.contains("value=\"ab\""), "keeps what was typed");
            assert_eq!(torrent(&pool).display_name, "Old name");
        }

        #[actix_web::test]
        async fn owner_deletes_and_loses_access() {
            let pool = pool();
            let (app, cookie) = app!(pool, Some(1));
            // Banning is moderator-only, so it does nothing for the owner
            let res =
                test::call_service(&app, post("/view/5/edit", &cookie, &[("ban", "Delete & Ban")]).to_request()).await;
            assert_eq!(location(&res), "/view/5/edit");
            assert_eq!(torrent(&pool).flags, TorrentFlags::TRUSTED.bits());

            let res =
                test::call_service(&app, post("/view/5/edit", &cookie, &[("delete", "Delete")]).to_request()).await;
            assert_eq!(location(&res), "/");
            assert!(torrent(&pool).is_deleted());
            let res = test::call_service(&app, get("/view/5/edit", &cookie).to_request()).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
        }

        #[actix_web::test]
        async fn moderator_bans_and_undeletes() {
            let pool = pool();
            let (app, cookie) = app!(pool, Some(3));
            let res =
                test::call_service(&app, post("/view/5/edit", &cookie, &[("ban", "Delete & Ban")]).to_request()).await;
            assert_eq!(location(&res), "/view/5");
            assert!(torrent(&pool).is_deleted() && torrent(&pool).is_banned());
            let stats: crate::models::Statistic =
                nyaa_statistics::table.find(5).first(&mut pool.get().unwrap()).unwrap();
            assert_eq!((stats.seed_count, stats.leech_count, stats.download_count), (0, 0, 9));

            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(page.contains("value=\"Undelete &amp; Unban\""), "{page}");

            test::call_service(&app, post("/view/5/edit", &cookie, &[("undelete", "Undelete & Unban")]).to_request())
                .await;
            assert_eq!(torrent(&pool).flags, TorrentFlags::TRUSTED.bits());
            assert_eq!(
                admin_logs(&pool),
                [
                    (3, "Torrent [#5](/view/5) has been deleted and banned".to_string()),
                    (3, "Torrent [#5](/view/5) has been undeleted and unbanned".to_string()),
                ]
            );
        }

        #[actix_web::test]
        async fn moderator_bans_torrent_and_uploader_from_the_edit_page() {
            let pool = pool();
            diesel::sql_query("UPDATE users SET last_login_ip = X'0000000000000000000000000A000007' WHERE id = 1")
                .execute(&mut pool.get().unwrap())
                .unwrap();
            let (app, cookie) = app!(pool, Some(3));
            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(page.contains("value=\"Delete &amp; Ban and Ban User\""), "{page}");
            assert!(page.contains("value=\"Delete &amp; Ban and Ban User+IP\""), "{page}");

            // A reason is required, and nothing changes without one
            let res = test::call_service(
                &app,
                post("/view/5/edit", &cookie, &[("ban_userip", "x"), ("reason", "")]).to_request(),
            )
            .await;
            assert_eq!(location(&res), "/view/5/edit");
            assert!(!torrent(&pool).is_deleted());

            let res = test::call_service(
                &app,
                post("/view/5/edit", &cookie, &[("ban_userip", "Delete & Ban and Ban User+IP"), ("reason", "spam")])
                    .to_request(),
            )
            .await;
            assert_eq!(location(&res), "/view/5");
            assert!(torrent(&pool).is_deleted() && torrent(&pool).is_banned());
            let mut conn = pool.get().unwrap();
            assert!(User::by_id(&mut conn, 1).unwrap().unwrap().is_banned());
            let bans: Vec<Ban> = bans::table.select(Ban::as_select()).load(&mut conn).unwrap();
            assert_eq!(bans.len(), 1);
            assert_eq!((bans[0].admin_id, bans[0].user_id), (3, Some(1)));
            assert_eq!(bans[0].ip_string().as_deref(), Some("10.0.0.7"));
            assert_eq!(bans[0].reason, "[Nyaa#5](/view/5) spam");
            drop(conn);
            assert_eq!(
                admin_logs(&pool),
                [
                    (3, "Torrent [#5](/view/5) has been deleted and banned".to_string()),
                    (
                        3,
                        "Uploader [owner](/user/owner) IP(10.0.0.7) of torrent [#5](/view/5) has been banned."
                            .to_string()
                    ),
                ]
            );

            // Everything is banned now, so the buttons are gone and a repeat does nothing
            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(
                page.contains("The uploader is <strong>IP banned</strong>") && !page.contains("name=\"ban_userip\""),
                "{page}"
            );
            test::call_service(
                &app,
                post("/view/5/edit", &cookie, &[("ban_user", "x"), ("reason", "again")]).to_request(),
            )
            .await;
            assert_eq!(bans::table.count().get_result::<i64>(&mut pool.get().unwrap()).unwrap(), 1);
        }

        #[actix_web::test]
        async fn uploader_ban_needs_a_higher_rank() {
            let pool = pool();
            diesel::sql_query("UPDATE users SET level = 2 WHERE id = 1").execute(&mut pool.get().unwrap()).unwrap();
            let (app, cookie) = app!(pool, Some(3));
            let page = String::from_utf8(
                test::call_and_read_body(&app, get("/view/5/edit", &cookie).to_request()).await.to_vec(),
            )
            .unwrap();
            assert!(!page.contains("name=\"ban_user\""), "{page}");
            let res = test::call_service(
                &app,
                post("/view/5/edit", &cookie, &[("ban_user", "x"), ("reason", "r")]).to_request(),
            )
            .await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN);
            assert!(!torrent(&pool).is_deleted());
        }

        fn admin_logs(pool: &DbPool) -> Vec<(i32, String)> {
            use crate::db::schema::adminlog;
            adminlog::table
                .order(adminlog::id)
                .select((adminlog::admin_id, adminlog::log))
                .load(&mut pool.get().unwrap())
                .unwrap()
        }

        #[actix_web::test]
        async fn comment_lock_changes_are_logged_and_owner_actions_are_not() {
            let pool = pool();
            let (app, cookie) = app!(pool, Some(3));
            let form = |locked: bool| {
                let mut f = vec![("display_name", "Old name"), ("category", "1_2"), ("submit", "Save")];
                if locked {
                    f.push(("is_comment_locked", "y"));
                }
                f
            };
            test::call_service(&app, post("/view/5/edit", &cookie, &form(true)).to_request()).await;
            test::call_service(&app, post("/view/5/edit", &cookie, &form(true)).to_request()).await;
            test::call_service(&app, post("/view/5/edit", &cookie, &form(false)).to_request()).await;
            assert_eq!(
                admin_logs(&pool),
                [
                    (3, "Torrent [#5](/view/5) marked as comments locked".to_string()),
                    (3, "Torrent [#5](/view/5) marked as comments unlocked".to_string()),
                ]
            );

            let (app, cookie) = app!(pool, Some(1));
            test::call_service(&app, post("/view/5/edit", &cookie, &[("delete", "Delete")]).to_request()).await;
            assert!(torrent(&pool).is_deleted());
            assert_eq!(admin_logs(&pool).len(), 2, "owners deleting their own torrent leave no log");
        }

        #[actix_web::test]
        async fn locked_comments_only_take_moderator_posts() {
            let pool = pool();
            let comments = |pool: &DbPool| -> Vec<(Option<i32>, String)> {
                nyaa_comments::table
                    .order(nyaa_comments::id)
                    .select((nyaa_comments::user_id, nyaa_comments::text))
                    .load(&mut pool.get().unwrap())
                    .unwrap()
            };

            // Unlocked: any logged-in user gets the form and can post
            let (app, cookie) = app!(pool, Some(2));
            let page =
                String::from_utf8(test::call_and_read_body(&app, get("/view/5", &cookie).to_request()).await.to_vec())
                    .unwrap();
            assert!(page.contains("name=\"comment\""), "{page}");
            let res =
                test::call_service(&app, post("/view/5", &cookie, &[("comment", "  first!  ")]).to_request()).await;
            assert_eq!(res.status(), StatusCode::FOUND);
            assert_eq!(location(&res), "/view/5#com-1");
            let res = test::call_service(&app, post("/view/5", &cookie, &[("comment", "ab")]).to_request()).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
            assert!(page.contains("Comment must be at least 3 characters"), "{page}");

            let (app, cookie) = app!(pool, None::<i32>);
            let res =
                test::call_service(&app, post("/view/5", &cookie, &[("comment", "anonymous")]).to_request()).await;
            assert_eq!(res.status(), StatusCode::FORBIDDEN);

            diesel::update(nyaa_torrents::table.find(5))
                .set(nyaa_torrents::flags.eq((TorrentFlags::TRUSTED | TorrentFlags::COMMENT_LOCKED).bits()))
                .execute(&mut pool.get().unwrap())
                .unwrap();

            // Locked: the uploader and other users see the notice but no form, and posts are refused
            for user in [1, 2] {
                let (app, cookie) = app!(pool, Some(user));
                let page = String::from_utf8(
                    test::call_and_read_body(&app, get("/view/5", &cookie).to_request()).await.to_vec(),
                )
                .unwrap();
                assert!(page.contains("Comments have been locked.") && !page.contains("name=\"comment\""), "{user}");
                let res =
                    test::call_service(&app, post("/view/5", &cookie, &[("comment", "let me in")]).to_request()).await;
                assert_eq!(res.status(), StatusCode::FORBIDDEN, "{user}");
            }

            // Moderators and above still comment, as upstream
            let (app, cookie) = app!(pool, Some(3));
            let page =
                String::from_utf8(test::call_and_read_body(&app, get("/view/5", &cookie).to_request()).await.to_vec())
                    .unwrap();
            assert!(page.contains("Comments have been locked.") && page.contains("name=\"comment\""), "{page}");
            let res =
                test::call_service(&app, post("/view/5", &cookie, &[("comment", "Locked, see rules")]).to_request())
                    .await;
            assert_eq!(location(&res), "/view/5#com-2");

            assert_eq!(comments(&pool), [(Some(2), "first!".to_string()), (Some(3), "Locked, see rules".to_string())]);
            assert_eq!(torrent(&pool).comment_count, 2);
        }

        /// The test row is given the hash of a real .torrent, so uploading that file collides with it.
        #[actix_web::test]
        async fn deleted_torrents_can_be_reuploaded_but_banned_cannot() {
            let info: &[u8] = b"d6:lengthi5e4:name5:a.txt12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
            let mut file = b"d4:info".to_vec();
            file.extend_from_slice(info);
            file.push(b'e');
            let hash = crate::torrent::parse_torrent(&file).unwrap().info_hash;

            let pool = pool();
            diesel::update(nyaa_torrents::table.find(5))
                .set(nyaa_torrents::info_hash.eq(&hash))
                .execute(&mut pool.get().unwrap())
                .unwrap();
            let (app, cookie) = app!(pool, Some(2));
            let upload = |file: &[u8]| {
                let mut body = b"--XX\r\nContent-Disposition: form-data; name=\"category\"\r\n\r\n1_2\r\n\
                                 --XX\r\nContent-Disposition: form-data; name=\"torrent_file\"; filename=\"a.torrent\"\r\n\
                                 Content-Type: application/x-bittorrent\r\n\r\n".to_vec();
                body.extend_from_slice(file);
                body.extend_from_slice(b"\r\n--XX--\r\n");
                test::TestRequest::post()
                    .uri("/upload")
                    .cookie(cookie.clone().unwrap())
                    .insert_header(("Content-Type", "multipart/form-data; boundary=XX"))
                    .set_payload(body)
                    .to_request()
            };
            let set_flags = |flags: TorrentFlags| {
                diesel::update(nyaa_torrents::table.find(5))
                    .set(nyaa_torrents::flags.eq(flags.bits()))
                    .execute(&mut pool.get().unwrap())
                    .unwrap()
            };

            let res = test::call_service(&app, upload(&file)).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
            assert!(page.contains("This torrent already exists (#5)"), "{page}");

            // Banned, deleted or not
            for flags in [TorrentFlags::BANNED, TorrentFlags::DELETED | TorrentFlags::BANNED] {
                set_flags(flags);
                let res = test::call_service(&app, upload(&file)).await;
                let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
                assert!(page.contains("This torrent is banned"), "{page}");
            }

            set_flags(TorrentFlags::DELETED);
            diesel::sql_query("INSERT INTO nyaa_comments (torrent_id, user_id, text) VALUES (5, 1, 'old')")
                .execute(&mut pool.get().unwrap())
                .unwrap();
            let res = test::call_service(&app, upload(&file)).await;
            assert_eq!(location(&res), "/view/5", "the reupload keeps the id");
            let t = torrent(&pool);
            assert_eq!((t.uploader_id, t.flags, t.display_name.as_str()), (Some(2), 0, "a.txt"));
            let comments: i64 = nyaa_comments::table.count().get_result(&mut pool.get().unwrap()).unwrap();
            assert_eq!(comments, 0);
            std::fs::remove_dir_all(config().torrent_storage_path).ok();
        }

        /// Multipart upload body with `fields` and, when given, a .torrent file.
        fn upload_body(fields: &[(&str, &str)], file: Option<&[u8]>) -> Vec<u8> {
            let mut body = Vec::new();
            for (name, value) in fields {
                body.extend_from_slice(
                    format!("--XX\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
                );
            }
            if let Some(file) = file {
                body.extend_from_slice(
                    b"--XX\r\nContent-Disposition: form-data; name=\"torrent_file\"; filename=\"c.torrent\"\r\n\r\n",
                );
                body.extend_from_slice(file);
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(b"--XX--\r\n");
            body
        }

        /// Upload runs the edit form's checks and shows every problem on the refilled form.
        #[actix_web::test]
        async fn invalid_uploads_rerender_the_form_with_errors() {
            let info: &[u8] = b"d6:lengthi5e4:name5:c.txt12:piece lengthi16384e6:pieces20:CCCCCCCCCCCCCCCCCCCCe";
            let mut file = b"d4:info".to_vec();
            file.extend_from_slice(info);
            file.push(b'e');
            let pool = pool();
            let (app, cookie) = app!(pool, Some(2));
            let post = |body: Vec<u8>| {
                test::TestRequest::post()
                    .uri("/upload")
                    .cookie(cookie.clone().unwrap())
                    .insert_header(("Content-Type", "multipart/form-data; boundary=XX"))
                    .set_payload(body)
                    .to_request()
            };
            let long_info = "i".repeat(256);
            type Case<'a> = (Vec<(&'a str, &'a str)>, Option<&'a [u8]>, &'a [&'a str]);
            let cases: [Case; 4] = [
                // A main category, which used to be stored as is
                (
                    vec![("category", "1_0"), ("display_name", "Kept name")],
                    Some(&file),
                    &["Please select a proper category", "value=\"Kept name\""],
                ),
                // Garbage used to fall back to Anime
                (vec![("category", "x")], Some(&file), &["Please select a category"]),
                (
                    vec![("category", "1_2"), ("information", &long_info)],
                    Some(&file),
                    &["Information must be at most 255"],
                ),
                (
                    vec![("category", "1_2"), ("display_name", "ab")],
                    None,
                    &["Please select a torrent file.", "at least 3 characters"],
                ),
            ];
            for (fields, file, expected) in cases {
                let res = test::call_service(&app, post(upload_body(&fields, file))).await;
                assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{fields:?}");
                let page = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
                for text in expected {
                    assert!(page.contains(text), "{fields:?}: {text}");
                }
            }
            let count: i64 = nyaa_torrents::table.count().get_result(&mut pool.get().unwrap()).unwrap();
            assert_eq!(count, 1, "nothing was stored");

            // The torrent's own name stands in for a blank display name
            let res = test::call_service(&app, post(upload_body(&[("category", "1_2")], Some(&file)))).await;
            assert_eq!(res.status(), StatusCode::FOUND);
            let t: Torrent =
                nyaa_torrents::table.order(nyaa_torrents::id.desc()).first(&mut pool.get().unwrap()).unwrap();
            assert_eq!((t.display_name.as_str(), t.main_category_id, t.sub_category_id), ("c.txt", 1, 2));
            std::fs::remove_dir_all(config().torrent_storage_path).ok();
        }

        /// A torrent whose info dict can't be stored is not left behind half-created.
        #[actix_web::test]
        async fn failed_file_write_removes_the_new_torrent() {
            let info: &[u8] = b"d6:lengthi5e4:name5:b.txt12:piece lengthi16384e6:pieces20:BBBBBBBBBBBBBBBBBBBBe";
            let mut file = b"d4:info".to_vec();
            file.extend_from_slice(info);
            file.push(b'e');
            let pool = pool();
            // The new torrent gets id 6, stored as 0/6.torrent.info; a file named 0 blocks it
            let dir = std::path::PathBuf::from(format!("{}-blocked", config().torrent_storage_path));
            let storage = Storage::local(dir.to_str().unwrap(), dir.to_str().unwrap()).unwrap();
            std::fs::remove_dir_all(dir.join("0")).ok();
            std::fs::write(dir.join("0"), b"in the way").unwrap();
            let (app, cookie) = app!(pool, Some(2), storage);

            let mut body = b"--XX\r\nContent-Disposition: form-data; name=\"category\"\r\n\r\n1_2\r\n\
                             --XX\r\nContent-Disposition: form-data; name=\"torrent_file\"; filename=\"b.torrent\"\r\n\
                             Content-Type: application/x-bittorrent\r\n\r\n"
                .to_vec();
            body.extend_from_slice(&file);
            body.extend_from_slice(b"\r\n--XX--\r\n");
            let res = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/upload")
                    .cookie(cookie.unwrap())
                    .insert_header(("Content-Type", "multipart/form-data; boundary=XX"))
                    .set_payload(body)
                    .to_request(),
            )
            .await;
            assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let mut conn = pool.get().unwrap();
            let torrents: i64 = nyaa_torrents::table.count().get_result(&mut conn).unwrap();
            let stats: i64 = nyaa_statistics::table.count().get_result(&mut conn).unwrap();
            assert_eq!((torrents, stats), (1, 1), "only the seeded torrent is left");
            std::fs::remove_dir_all(dir).ok();
        }
    }
}
