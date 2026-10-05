use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::utils::context::base_context;
use crate::middleware::auth::get_current_user;
use crate::models::User;
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

    let html = tmpl.render("user.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

/// An uploaded avatar. Links carry `?v=` with the upload time, so a new one shows at once.
pub async fn avatar(
    cfg: web::Data<Config>,
    path: web::Path<i32>,
) -> Result<HttpResponse> {
    let file = crate::utils::avatar::path(&cfg, path.into_inner());
    let data = tokio::fs::read(file).await
        .map_err(|_| actix_web::error::ErrorNotFound("No avatar"))?;
    Ok(HttpResponse::Ok()
        .content_type("image/png")
        .insert_header(("Cache-Control", "public, max-age=86400"))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .body(data))
}
