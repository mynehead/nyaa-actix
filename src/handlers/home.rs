use actix_session::Session;
use actix_web::{web, HttpResponse, Result};
use serde::Deserialize;
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::models::Banner;
use crate::search::db::{with_stats, SearchQuery};
use crate::search::search;
use crate::utils::context::base_context;
use crate::utils::context::SearchState;
use crate::utils::internal_error;
use crate::utils::pagination::Pagination;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub s: Option<String>,
    pub o: Option<String>,
    pub c: Option<String>,
    pub f: Option<String>,
    pub p: Option<i64>,
}

pub async fn home(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
    params: web::Query<SearchParams>,
) -> Result<HttpResponse> {
    let current_user = get_current_user(&session, &pool);
    let is_admin = current_user.as_ref().map(|u| u.is_moderator()).unwrap_or(false);

    let q = SearchQuery::from_params(
        params.q.clone(),
        None,
        None,
        params.c.as_deref(),
        params.f.as_deref(),
        params.s.as_deref(),
        params.o.as_deref(),
        params.p,
        cfg.results_per_page,
        is_admin,
    );

    let mut conn = pool.get().map_err(internal_error)?;

    let result = search(&mut conn, cfg.meili.as_ref(), &q).map_err(internal_error)?;

    let pagination = Pagination::new(q.page, result.total, q.per_page);

    let mut ctx = base_context(&cfg, current_user.as_ref());
    let torrents = with_stats(&mut conn, result.torrents).map_err(internal_error)?;
    ctx.insert("torrents", &torrents);
    ctx.insert("pagination", &pagination);
    ctx.insert("banners", &Banner::active(&mut conn).map_err(internal_error)?);
    ctx.insert("search", &SearchState::new(&params.q, &params.c, &params.f, &params.s, &params.o));

    let html = tmpl.render("home.html", &ctx).map_err(internal_error)?;

    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}
