use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::utils::context::base_context;
use crate::middleware::auth::get_current_user;
use crate::db::schema::{bans, nyaa_comments, nyaa_statistics, nyaa_torrents, users};
use crate::models::{user_link, AdminLog, Ban, NewBan, TorrentFlags, User, UserStatus, MAX_BAN_REASON_LEN};
use crate::utils::{flash, sanitize_text, unpack_ip};
use diesel::prelude::*;
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::utils::context::SearchState;
use crate::utils::pagination::Pagination;

#[derive(Debug, Deserialize)]
pub struct UserSearchParams {
    pub q: Option<String>,
    pub s: Option<String>,
    pub o: Option<String>,
    pub c: Option<String>,
    pub f: Option<String>,
    pub p: Option<i64>,
}

pub async fn view_user(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    params: web::Query<UserSearchParams>,
) -> Result<HttpResponse> {
    let username = path.into_inner();
    let current_user = get_current_user(&session, &pool);
    let is_admin = current_user.as_ref().map(|u| u.is_moderator()).unwrap_or(false);

    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let profile_user = User::by_username(&mut conn, &username)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;

    let mut q = SearchQuery::from_params(
        params.q.clone(),
        Some(profile_user.id),
        None,
        params.c.as_deref(),
        params.f.as_deref(),
        params.s.as_deref(),
        params.o.as_deref(),
        params.p,
        cfg.results_per_page,
        is_admin,
    );
    // Owners and moderators see everything on the profile; everyone else
    // sees neither hidden nor anonymous uploads.
    let is_owner = current_user.as_ref().map(|u| u.id == profile_user.id).unwrap_or(false);
    q.include_hidden = is_admin || is_owner;
    q.hide_anonymous = !(is_admin || is_owner);

    let result = search(&mut conn, cfg.meili.as_ref(), &q)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let pagination = Pagination::new(q.page, result.total, q.per_page);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("profile_user", &profile_user);
    ctx.insert("avatar_url", &profile_user.avatar_url(&cfg));
    let torrents = with_stats(&mut conn, result.torrents)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("search", &SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o));
    // The navbar search scopes itself to this user, as upstream's user_page does
    ctx.insert("user_page", &true);
    ctx.insert("flash_messages", &flash::take(&session));
    if let Some(moderator) = current_user.as_ref().filter(|m| can_ban(m, &profile_user)) {
        let bans = Ban::banned(&mut conn, Some(profile_user.id), profile_user.last_login_ip.as_deref())
            .map_err(actix_web::error::ErrorInternalServerError)?;
        let ip_banned = bans.iter().any(|b| b.user_ip.is_some() && b.user_ip == profile_user.last_login_ip);
        let bans = Ban::with_names(&mut conn, bans).map_err(actix_web::error::ErrorInternalServerError)?;
        ctx.insert("ban_form", &true);
        ctx.insert("bans", &bans);
        ctx.insert("ip_banned", &ip_banned);
        if moderator.is_superadmin() {
            let ip = |b: &Option<Vec<u8>>| b.as_deref().and_then(unpack_ip).map(|ip| ip.to_string());
            ctx.insert("last_login_ip", &ip(&profile_user.last_login_ip));
            ctx.insert("registration_ip", &ip(&profile_user.registration_ip));
        }
    }

    let html = tmpl.render("user.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// An uploaded avatar. Links carry `?v=` with the upload time, so a new one shows at once.
pub async fn avatar(
    storage: web::Data<crate::storage::Storage>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let data = storage.get(crate::storage::Kind::Avatar, path.into_inner()).await
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("No avatar"))?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        .insert_header(("Cache-Control", "public, max-age=86400"))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .body(data))
}

/// Upstream shows the user page's Danger Zone to moderators above the user's level.
fn can_ban(moderator: &User, user: &User) -> bool {
    moderator.is_moderator() && moderator.level > user.level
}

/// The user page's Danger Zone form (upstream `BanForm`): one of the three buttons, and
/// a reason for the bans.
#[derive(Debug, Default, Deserialize)]
pub struct BanForm {
    #[serde(default)]
    pub reason: String,
    pub ban_user: Option<String>,
    pub ban_userip: Option<String>,
    pub unban: Option<String>,
}

/// Bans or unbans the user, as upstream's `view_user` POST.
pub async fn ban_user_post(
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
    form: web::Form<BanForm>,
) -> Result<HttpResponse> {
    let moderator = get_current_user(&session, &pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let user = User::by_username(&mut conn, &path.into_inner())
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;
    if !can_ban(&moderator, &user) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let url = format!("/user/{}", user.username);
    let back = || HttpResponse::SeeOther().insert_header(("Location", url.clone())).finish();

    let bans = Ban::banned(&mut conn, Some(user.id), user.last_login_ip.as_deref())
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let ip_banned = bans.iter().any(|b| b.user_ip.is_some() && b.user_ip == user.last_login_ip);
    let unban = form.unban.is_some();
    let ban_ip = !unban && form.ban_userip.is_some();
    if !unban && !ban_ip && form.ban_user.is_none() {
        return Ok(back());
    }
    // Buttons that don't apply to this user's current state (upstream flashes the same)
    let pointless = (form.ban_user.is_some() && !ban_ip && !unban && user.is_banned())
        || (ban_ip && ip_banned)
        || (unban && !user.is_banned() && bans.is_empty());
    if pointless {
        flash::push(&session, "danger", "", "That action doesn't apply to this user.");
        return Ok(back());
    }
    let reason = form.reason.trim();
    if !unban {
        if reason.is_empty() {
            flash::push(&session, "danger", "Ban failed!", "Please specify a ban reason.");
            return Ok(back());
        }
        if reason.chars().count() > MAX_BAN_REASON_LEN {
            flash::push(&session, "danger", "Ban failed!",
                &format!("Reason must be at most {} characters long.", MAX_BAN_REASON_LEN));
            return Ok(back());
        }
        if ban_ip {
            match user.last_login_ip.as_deref().and_then(unpack_ip) {
                None => {
                    flash::push(&session, "danger", "Ban failed!", "This user has no known IP to ban.");
                    return Ok(back());
                }
                // Behind a reverse proxy every visitor looks like loopback; banning it locks out the site
                Some(ip) if ip.is_loopback() => {
                    flash::push(&session, "danger", "Ban failed!",
                        "This user's IP is a loopback address, which would ban everyone behind the proxy.");
                    return Ok(back());
                }
                Some(_) => {}
            }
        }
    }

    let action = if unban { "unbanned" } else { "banned" };
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        let mut user_str = user_link(&user.username);
        let status = if unban { UserStatus::Active } else { UserStatus::Banned };
        diesel::update(users::table.find(user.id))
            .set(users::status.eq(status as i32))
            .execute(conn)?;
        if unban {
            for ban in &bans {
                if let Some(ip) = ban.ip_string() {
                    user_str.push_str(&format!(" IP({})", ip));
                }
                diesel::delete(bans::table.find(ban.id)).execute(conn)?;
            }
        } else {
            let user_ip = if ban_ip { user.last_login_ip.clone() } else { None };
            if let Some(ip) = user_ip.as_deref().and_then(unpack_ip) {
                user_str.push_str(&format!(" IP({})", ip));
            }
            diesel::insert_into(bans::table).values(NewBan {
                created_time: chrono::Utc::now().naive_utc(),
                admin_id: moderator.id,
                user_id: Some(user.id),
                user_ip,
                reason: sanitize_text(reason),
            }).execute(conn)?;
        }
        AdminLog::add(conn, moderator.id, &format!("User {} has been {}.", user_str, action))
    }).map_err(actix_web::error::ErrorInternalServerError)?;
    flash::push(&session, "success", "", &format!("User has been successfully {}.", action));
    Ok(back())
}

/// The user behind a nuke request; superadmins only, and only on users below them.
fn nuke_target(session: &Session, pool: &DbPool, username: &str) -> Result<(User, User, diesel::r2d2::PooledConnection<crate::db::DbManager>)> {
    let admin = get_current_user(session, pool)
        .ok_or_else(|| actix_web::error::ErrorUnauthorized("Login required"))?;
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let user = User::by_username(&mut conn, username)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;
    if !admin.is_superadmin() || !can_ban(&admin, &user) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok((admin, user, conn))
}

/// "Nuke Torrents": deletes and bans every torrent of the user, as upstream's
/// `nuke_user_torrents`.
pub async fn nuke_torrents_post(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let (admin, user, mut conn) = nuke_target(&session, &pool, &path.into_inner())?;
    let ids: Vec<i32> = conn.transaction::<_, diesel::result::Error, _>(|conn| {
        let torrents: Vec<(i32, i32)> = nyaa_torrents::table
            .filter(nyaa_torrents::uploader_id.eq(user.id))
            .select((nyaa_torrents::id, nyaa_torrents::flags))
            .load(conn)?;
        let banned = (TorrentFlags::DELETED | TorrentFlags::BANNED).bits();
        for &(id, flags) in &torrents {
            diesel::update(nyaa_torrents::table.find(id))
                .set(nyaa_torrents::flags.eq(flags | banned))
                .execute(conn)?;
        }
        let ids: Vec<i32> = torrents.into_iter().map(|(id, _)| id).collect();
        diesel::update(nyaa_statistics::table.filter(nyaa_statistics::torrent_id.eq_any(&ids)))
            .set((nyaa_statistics::seed_count.eq(0), nyaa_statistics::leech_count.eq(0)))
            .execute(conn)?;
        if !ids.is_empty() {
            AdminLog::add(conn, admin.id, &format!("Nuked {} torrents of {}", ids.len(), user_link(&user.username)))?;
        }
        Ok(ids)
    }).map_err(actix_web::error::ErrorInternalServerError)?;
    for id in ids {
        crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), id);
    }
    flash::push(&session, "success", "", &format!("Torrents of {} have been nuked.", user.username));
    Ok(HttpResponse::SeeOther().insert_header(("Location", format!("/user/{}", user.username))).finish())
}

/// "Nuke Comments": deletes every comment of the user, as upstream's `nuke_user_comments`.
pub async fn nuke_comments_post(
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let (admin, user, mut conn) = nuke_target(&session, &pool, &path.into_inner())?;
    let torrent_ids: Vec<i32> = conn.transaction::<_, diesel::result::Error, _>(|conn| {
        let mut torrent_ids: Vec<i32> = nyaa_comments::table
            .filter(nyaa_comments::user_id.eq(user.id))
            .select(nyaa_comments::torrent_id)
            .load(conn)?;
        let deleted = diesel::delete(nyaa_comments::table.filter(nyaa_comments::user_id.eq(user.id)))
            .execute(conn)?;
        torrent_ids.sort_unstable();
        torrent_ids.dedup();
        for &tid in &torrent_ids {
            let count: i64 = nyaa_comments::table.filter(nyaa_comments::torrent_id.eq(tid)).count().get_result(conn)?;
            diesel::update(nyaa_torrents::table.find(tid))
                .set(nyaa_torrents::comment_count.eq(count as i32))
                .execute(conn)?;
        }
        if deleted > 0 {
            AdminLog::add(conn, admin.id, &format!("Nuked {} comments of {}", deleted, user_link(&user.username)))?;
        }
        Ok(torrent_ids)
    }).map_err(actix_web::error::ErrorInternalServerError)?;
    for id in torrent_ids {
        crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), id);
    }
    flash::push(&session, "success", "", &format!("Comments of {} have been nuked.", user.username));
    Ok(HttpResponse::SeeOther().insert_header(("Location", format!("/user/{}", user.username))).finish())
}
