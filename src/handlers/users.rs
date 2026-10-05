use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::User;
use crate::search::db::{search, SearchQuery};
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

    let result = search(&mut conn, &q)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    let pagination = Pagination::new(q.page, result.total, q.per_page);

    let mut ctx = tera::Context::new();
    ctx.insert("current_user", &current_user);
    ctx.insert("profile_user", &profile_user);
    ctx.insert("torrents", &result.torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("search_term", &params.q);
    ctx.insert("config", &serde_json::json!({
        "site_name": cfg.site_name,
        "site_flavor": cfg.site_flavor,
    }));

    let html = tmpl.render("user.html", &ctx)
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}
