use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::{Deserialize, Serialize};
use tera::Tera;

use crate::auth::policy::can_ban;
use crate::auth::{CurrentUser, LoggedIn, Moderator, Permission};
use crate::config::Config;
use crate::db::schema::{bans, nyaa_comments, nyaa_statistics, nyaa_torrents, users};
use crate::db::DbPool;
use crate::models::{
    user_link, AdminLog, Ban, Comment, NewBan, TorrentFlags, User, UserLevel, UserStatus, MAX_BAN_REASON_LEN,
};
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::utils::context::base_context;
use crate::utils::context::SearchState;
use crate::utils::pagination::Pagination;
use crate::utils::{flash, internal_error, sanitize_text, unpack_ip};
use diesel::prelude::*;

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
    CurrentUser(current_user): CurrentUser,
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    params: web::Query<UserSearchParams>,
) -> Result<HttpResponse> {
    let username = path.into_inner();
    let moderator = current_user.as_ref().is_some_and(|u| u.can(Permission::ModerateTorrents));

    let mut conn = pool.get().map_err(internal_error)?;
    let profile_user = User::by_username(&mut conn, &username)
        .map_err(internal_error)?
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
        moderator,
    );
    // Owners and moderators see everything on the profile; everyone else
    // sees neither hidden nor anonymous uploads.
    let is_owner = current_user.as_ref().map(|u| u.id == profile_user.id).unwrap_or(false);
    q.include_hidden = moderator || is_owner;
    q.hide_anonymous = !(moderator || is_owner);

    crate::utils::pagination::check_max_pages(q.page, cfg.max_pages)?;
    let result = search(&mut conn, cfg.meili.as_ref(), &q).map_err(internal_error)?;
    let pagination = Pagination::capped(q.page, result.total, q.per_page, cfg.max_pages);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    ctx.insert("profile_user", &profile_user);
    ctx.insert("avatar_url", &profile_user.avatar_url(&cfg));
    let torrents = with_stats(&mut conn, result.torrents).map_err(internal_error)?;
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("search", &SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o));
    // The navbar search scopes itself to this user, as upstream's user_page does
    ctx.insert("user_page", &true);
    ctx.insert("flash_messages", &flash::take(&session));
    if let Some(moderator) = current_user.as_ref().filter(|m| can_ban(m, &profile_user)) {
        let bans = Ban::banned(&mut conn, Some(profile_user.id), profile_user.last_login_ip.as_deref())
            .map_err(internal_error)?;
        let ip_banned = bans.iter().any(|b| b.user_ip.is_some() && b.user_ip == profile_user.last_login_ip);
        let bans = Ban::with_names(&mut conn, bans).map_err(internal_error)?;
        let (default, choices) = user_class_choices(moderator, &profile_user);
        ctx.insert("admin_form", &true);
        ctx.insert("user_class_choices", &choices);
        ctx.insert("user_class_default", default);
        ctx.insert("ban_form", &true);
        ctx.insert("bans", &bans);
        ctx.insert("ip_banned", &ip_banned);
        if moderator.can(Permission::ResetTwoFactor) {
            let two_factor =
                crate::auth::mfa::UserMfa::is_enabled(&mut conn, profile_user.id).map_err(internal_error)?;
            ctx.insert("profile_user_two_factor", &two_factor);
        }
        if moderator.can(Permission::SeeIps) {
            let ip = |b: &Option<Vec<u8>>| b.as_deref().and_then(unpack_ip).map(|ip| ip.to_string());
            ctx.insert("last_login_ip", &ip(&profile_user.last_login_ip));
            ctx.insert("registration_ip", &ip(&profile_user.registration_ip));
        }
    }

    let html = tmpl.render("user.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// An uploaded avatar. Links carry `?v=` with the upload time, so a new one shows at once.
pub async fn avatar(storage: web::Data<crate::storage::Storage>, path: web::Path<i32>) -> Result<HttpResponse> {
    let data = storage
        .get(crate::storage::Kind::Avatar, path.into_inner())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("No avatar"))?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        .insert_header(("Cache-Control", "public, max-age=86400"))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .body(data))
}

/// One option of the "Change User Class" menu.
#[derive(Debug, Serialize)]
struct UserClassChoice {
    value: &'static str,
    label: &'static str,
    level: i32,
}

/// The classes `moderator` may give `user`, and the one selected now, as upstream's
/// `_create_user_class_choices`: moderators pick Regular or Trusted, superadmins also
/// Moderator. Nobody can make another superadmin here.
fn user_class_choices(moderator: &User, user: &User) -> (&'static str, Vec<UserClassChoice>) {
    let mut choices = vec![UserClassChoice { value: "regular", label: "Regular", level: UserLevel::Regular as i32 }];
    if moderator.can(Permission::ChangeUserClass) {
        choices.push(UserClassChoice { value: "trusted", label: "Trusted", level: UserLevel::Trusted as i32 });
    }
    if moderator.can(Permission::GrantModerator) {
        choices.push(UserClassChoice { value: "moderator", label: "Moderator", level: UserLevel::Moderator as i32 });
    }
    let default = if user.level() >= UserLevel::Moderator {
        "moderator"
    } else if user.level() >= UserLevel::Trusted {
        "trusted"
    } else {
        "regular"
    };
    (default, choices)
}

/// The user page's two admin forms, posted to the same URL as upstream: the Danger Zone
/// (`BanForm`: one of the three buttons, and a reason for the bans) and "Change User
/// Class" (`UserForm`: the new class, or Activate User).
#[derive(Debug, Default, Deserialize)]
pub struct BanForm {
    #[serde(default)]
    pub reason: String,
    pub ban_user: Option<String>,
    pub ban_userip: Option<String>,
    pub unban: Option<String>,
    pub user_class: Option<String>,
    pub activate_user: Option<String>,
}

/// Applies the "Change User Class" form, as upstream's `view_user` POST does when no ban
/// button was pressed.
fn change_user_class(
    session: &Session,
    conn: &mut crate::db::DbConnection,
    moderator: &User,
    user: &User,
    form: &BanForm,
) -> Result<()> {
    let (_, choices) = user_class_choices(moderator, user);
    let level = match form.user_class.as_deref() {
        Some(value) => match choices.iter().find(|c| c.value == value) {
            Some(choice) => Some((choice.value, choice.level)),
            None => {
                flash::push(session, "danger", "", "Please select a proper user class");
                return Ok(());
            }
        },
        None => None,
    };
    let activate = form.activate_user.is_some() && !user.is_banned() && !user.is_active();
    let link = user_link(&user.username);
    conn.transaction::<_, diesel::result::Error, _>(|conn| {
        if let Some((value, level)) = level.filter(|&(_, level)| level != user.level) {
            diesel::update(users::table.find(user.id)).set(users::level.eq(level)).execute(conn)?;
            AdminLog::add(conn, moderator.id, &format!("{} changed to {} user", link, value))?;
        }
        if activate {
            diesel::update(users::table.find(user.id))
                .set(users::status.eq(UserStatus::Active as i32))
                .execute(conn)?;
            AdminLog::add(conn, moderator.id, &format!("{} was manually activated", link))?;
        }
        Ok(())
    })
    .map_err(internal_error)?;
    if activate {
        flash::push(session, "success", "", &format!("{} was manually activated", user.username));
    }
    Ok(())
}

/// Bans or unbans the user, as upstream's `view_user` POST.
pub async fn ban_user_post(
    LoggedIn(moderator): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    path: web::Path<String>,
    form: web::Form<BanForm>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let user = User::by_username(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;
    if !can_ban(&moderator, &user) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    let url = format!("/user/{}", urlencoding::encode(&user.username));
    let back = || HttpResponse::SeeOther().insert_header(("Location", url.clone())).finish();

    if form.ban_user.is_none() && form.ban_userip.is_none() && form.unban.is_none() {
        change_user_class(&session, &mut conn, &moderator, &user, &form)?;
        return Ok(back());
    }

    let bans = Ban::banned(&mut conn, Some(user.id), user.last_login_ip.as_deref()).map_err(internal_error)?;
    let ip_banned = bans.iter().any(|b| b.user_ip.is_some() && b.user_ip == user.last_login_ip);
    let unban = form.unban.is_some();
    let ban_ip = !unban && form.ban_userip.is_some();
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
            flash::push(
                &session,
                "danger",
                "Ban failed!",
                &format!("Reason must be at most {} characters long.", MAX_BAN_REASON_LEN),
            );
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
                    flash::push(
                        &session,
                        "danger",
                        "Ban failed!",
                        "This user's IP is a loopback address, which would ban everyone behind the proxy.",
                    );
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
        diesel::update(users::table.find(user.id)).set(users::status.eq(status as i32)).execute(conn)?;
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
            diesel::insert_into(bans::table)
                .values(NewBan {
                    created_time: chrono::Utc::now().naive_utc(),
                    admin_id: moderator.id,
                    user_id: Some(user.id),
                    user_ip,
                    reason: sanitize_text(reason),
                })
                .execute(conn)?;
        }
        AdminLog::add(conn, moderator.id, &format!("User {} has been {}.", user_str, action))
    })
    .map_err(internal_error)?;
    flash::push(&session, "success", "", &format!("User has been successfully {}.", action));
    Ok(back())
}

/// The user behind a nuke request; superadmins only, and only on users below them.
fn nuke_target(
    admin: User,
    pool: &DbPool,
    username: &str,
) -> Result<(User, User, diesel::r2d2::PooledConnection<crate::db::DbManager>)> {
    let mut conn = pool.get().map_err(actix_web::error::ErrorInternalServerError)?;
    let user = User::by_username(&mut conn, username)
        .map_err(actix_web::error::ErrorInternalServerError)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;
    if !admin.can(Permission::NukeUsers) || !can_ban(&admin, &user) {
        return Err(actix_web::error::ErrorForbidden("Not allowed"));
    }
    Ok((admin, user, conn))
}

/// "Nuke Torrents": deletes and bans every torrent of the user, as upstream's
/// `nuke_user_torrents`.
pub async fn nuke_torrents_post(
    LoggedIn(admin): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let (admin, user, mut conn) = nuke_target(admin, &pool, &path.into_inner())?;
    let ids: Vec<i32> = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
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
                AdminLog::add(
                    conn,
                    admin.id,
                    &format!("Nuked {} torrents of {}", ids.len(), user_link(&user.username)),
                )?;
            }
            Ok(ids)
        })
        .map_err(actix_web::error::ErrorInternalServerError)?;
    for id in ids {
        crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), id);
        crate::tracker::torrent_changed(cfg.tracker.as_ref(), id);
    }
    flash::push(&session, "success", "", &format!("Torrents of {} have been nuked.", user.username));
    Ok(HttpResponse::SeeOther().insert_header(("Location", format!("/user/{}", user.username))).finish())
}

/// "Nuke Comments": deletes every comment of the user, as upstream's `nuke_user_comments`.
pub async fn nuke_comments_post(
    LoggedIn(admin): LoggedIn,
    session: Session,
    pool: web::Data<DbPool>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
) -> Result<HttpResponse> {
    let (admin, user, mut conn) = nuke_target(admin, &pool, &path.into_inner())?;
    let torrent_ids: Vec<i32> = conn
        .transaction::<_, diesel::result::Error, _>(|conn| {
            let mut torrent_ids: Vec<i32> = nyaa_comments::table
                .filter(nyaa_comments::user_id.eq(user.id))
                .select(nyaa_comments::torrent_id)
                .load(conn)?;
            let deleted =
                diesel::delete(nyaa_comments::table.filter(nyaa_comments::user_id.eq(user.id))).execute(conn)?;
            torrent_ids.sort_unstable();
            torrent_ids.dedup();
            for &tid in &torrent_ids {
                let count: i64 =
                    nyaa_comments::table.filter(nyaa_comments::torrent_id.eq(tid)).count().get_result(conn)?;
                diesel::update(nyaa_torrents::table.find(tid))
                    .set(nyaa_torrents::comment_count.eq(count as i32))
                    .execute(conn)?;
            }
            if deleted > 0 {
                AdminLog::add(conn, admin.id, &format!("Nuked {} comments of {}", deleted, user_link(&user.username)))?;
            }
            Ok(torrent_ids)
        })
        .map_err(actix_web::error::ErrorInternalServerError)?;
    for id in torrent_ids {
        crate::search::index::torrent_changed(&mut conn, cfg.meili.as_ref(), id);
    }
    flash::push(&session, "success", "", &format!("Comments of {} have been nuked.", user.username));
    Ok(HttpResponse::SeeOther().insert_header(("Location", format!("/user/{}", user.username))).finish())
}

#[derive(Debug, Deserialize)]
pub struct CommentsParams {
    pub p: Option<i64>,
}

/// Upstream's user comments page shows 100 a page.
const COMMENTS_PER_PAGE: i64 = 100;

/// /user/{name}/comments: every comment the user wrote, newest first, as upstream's
/// `view_user_comments`. Moderators only, as upstream ("for now").
pub async fn view_user_comments(
    Moderator(current_user): Moderator,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    path: web::Path<String>,
    params: web::Query<CommentsParams>,
) -> Result<HttpResponse> {
    let mut conn = pool.get().map_err(internal_error)?;
    let user = User::by_username(&mut conn, &path.into_inner())
        .map_err(internal_error)?
        .ok_or_else(|| actix_web::error::ErrorNotFound("User not found"))?;

    let total: i64 = nyaa_comments::table
        .filter(nyaa_comments::user_id.eq(user.id))
        .count()
        .get_result(&mut conn)
        .map_err(internal_error)?;
    let pagination = Pagination::new(params.p.unwrap_or(1), total, COMMENTS_PER_PAGE);
    let comments: Vec<Comment> = nyaa_comments::table
        .filter(nyaa_comments::user_id.eq(user.id))
        .order((nyaa_comments::created_time.desc(), nyaa_comments::id.desc()))
        .offset((pagination.current - 1) * COMMENTS_PER_PAGE)
        .limit(COMMENTS_PER_PAGE)
        .select(Comment::as_select())
        .load(&mut conn)
        .map_err(internal_error)?;
    let torrent_ids: Vec<i32> = comments.iter().map(|c| c.torrent_id).collect();
    let names: std::collections::HashMap<i32, String> = nyaa_torrents::table
        .filter(nyaa_torrents::id.eq_any(&torrent_ids))
        .select((nyaa_torrents::id, nyaa_torrents::display_name))
        .load::<(i32, String)>(&mut conn)
        .map_err(internal_error)?
        .into_iter()
        .collect();
    let comments: Vec<serde_json::Value> = comments
        .into_iter()
        .map(|c| {
            let torrent_name = names.get(&c.torrent_id).cloned().unwrap_or_default();
            serde_json::json!({ "comment": c, "torrent_name": torrent_name })
        })
        .collect();

    let mut ctx = base_context(&cfg, Some(&current_user));
    ctx.insert("profile_user", &user);
    ctx.insert("avatar_url", &user.avatar_url(&cfg));
    ctx.insert("comments", &comments);
    ctx.insert("pagination", &pagination);
    let html = tmpl.render("user_comments.html", &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::auth::test_support::login;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::{
        cookie::{Cookie, Key},
        http::StatusCode,
        test, App,
    };
    use diesel::r2d2::Pool;

    fn pool() -> DbPool {
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        let mut conn = pool.get().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
             (1, 'alice', 'x', 0, 0), (2, 'mod', 'x', 1, 2), (3, 'admin', 'x', 1, 3), (4, 'mod2', 'x', 1, 2)",
        )
        .execute(&mut conn)
        .unwrap();
        diesel::sql_query(format!(
            "INSERT INTO nyaa_torrents (id, info_hash, display_name, torrent_name, information, description, \
             flags, uploader_id, main_category_id, sub_category_id) \
             VALUES (5, X'{}', 'Some torrent', 's.torrent', '', '', 0, 2, 1, 2)",
            "ab".repeat(20)
        ))
        .execute(&mut conn)
        .unwrap();
        diesel::sql_query("INSERT INTO nyaa_comments (torrent_id, user_id, text) VALUES (5, 1, 'hello there')")
            .execute(&mut conn)
            .unwrap();
        pool
    }

    fn config() -> Config {
        Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: String::new(),
            enable_gravatar: false,
            gravatar_url: crate::config::DEFAULT_GRAVATAR_URL.into(),
            gravatar_sha256: false,
            maintenance: Default::default(),
            site_url: String::new(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            upload_limit: Default::default(),
            mail: Default::default(),
            trusted: Default::default(),
            tickets: Default::default(),
            mfa: Default::default(),
        }
    }

    macro_rules! app {
        ($pool:expr, $user:expr) => {{
            let mut tera = Tera::new("templates/**/*").unwrap();
            crate::utils::tera_filters::register(&mut tera);
            let app = test::init_service(
                App::new()
                    .app_data(web::Data::new(config()))
                    .app_data(web::Data::new($pool.clone()))
                    .app_data(web::Data::new(tera))
                    .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                    .route("/login/{id}", web::get().to(login))
                    .route("/user/{username}", web::get().to(view_user))
                    .route("/user/{username}", web::post().to(ban_user_post))
                    .route("/user/{username}/comments", web::get().to(view_user_comments)),
            )
            .await;
            let res =
                test::call_service(&app, test::TestRequest::get().uri(&format!("/login/{}", $user)).to_request()).await;
            let cookie: Cookie<'static> = res.response().cookies().next().unwrap().into_owned();
            (app, cookie)
        }};
    }

    fn user(pool: &DbPool, id: i32) -> User {
        User::by_id(&mut pool.get().unwrap(), id).unwrap().unwrap()
    }

    fn logs(pool: &DbPool) -> Vec<String> {
        use crate::db::schema::adminlog;
        adminlog::table.order(adminlog::id).select(adminlog::log).load(&mut pool.get().unwrap()).unwrap()
    }

    #[actix_web::test]
    async fn moderator_sees_class_menu_without_moderator_option() {
        let pool = pool();
        let (app, cookie) = app!(pool, 2);
        let req = test::TestRequest::get().uri("/user/alice").cookie(cookie).to_request();
        let page = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(page.contains("Change User Class"), "{page}");
        assert!(page.contains("value=\"trusted\""));
        assert!(!page.contains("value=\"moderator\""));
        assert!(page.contains("Activate User"));
        assert!(page.contains("/user/alice/comments"));
    }

    #[actix_web::test]
    async fn moderator_cannot_promote_to_moderator_or_touch_peers() {
        let pool = pool();
        let (app, cookie) = app!(pool, 2);
        let post = |uri: &str, form: &[(&str, &str)]| {
            test::TestRequest::post().uri(uri).cookie(cookie.clone()).set_form(form).to_request()
        };
        test::call_service(&app, post("/user/alice", &[("user_class", "moderator")])).await;
        assert_eq!(user(&pool, 1).level, 0);
        let res = test::call_service(&app, post("/user/mod2", &[("user_class", "regular")])).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(user(&pool, 4).level, 2);

        test::call_service(&app, post("/user/alice", &[("user_class", "trusted")])).await;
        assert_eq!(user(&pool, 1).level, 1);
        assert_eq!(logs(&pool), ["[alice](/user/alice) changed to trusted user"]);
    }

    #[actix_web::test]
    async fn superadmin_promotes_and_activates() {
        let pool = pool();
        let (app, cookie) = app!(pool, 3);
        let req = test::TestRequest::post()
            .uri("/user/alice")
            .cookie(cookie.clone())
            .set_form([("user_class", "moderator"), ("activate_user", "Activate User")])
            .to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.headers().get("Location").unwrap(), "/user/alice");
        let alice = user(&pool, 1);
        assert_eq!((alice.level, alice.status), (2, UserStatus::Active as i32));
        assert_eq!(
            logs(&pool),
            ["[alice](/user/alice) changed to moderator user", "[alice](/user/alice) was manually activated"]
        );
        // Same class again changes and logs nothing
        let req = test::TestRequest::post()
            .uri("/user/alice")
            .cookie(cookie)
            .set_form([("user_class", "moderator")])
            .to_request();
        test::call_service(&app, req).await;
        assert_eq!(logs(&pool).len(), 2);
    }

    #[actix_web::test]
    async fn comments_page_is_for_moderators() {
        let pool = pool();
        let (app, cookie) = app!(pool, 2);
        let req = test::TestRequest::get().uri("/user/alice/comments").cookie(cookie).to_request();
        let page = String::from_utf8(test::call_and_read_body(&app, req).await.to_vec()).unwrap();
        assert!(page.contains("hello there") && page.contains("Some torrent"), "{page}");

        // alice is inactive, so she counts as a guest: 401, like the other moderator pages
        let (app, cookie) = app!(pool, 1);
        let req = test::TestRequest::get().uri("/user/alice/comments").cookie(cookie).to_request();
        assert_eq!(test::call_service(&app, req).await.status(), StatusCode::UNAUTHORIZED);
    }
}
