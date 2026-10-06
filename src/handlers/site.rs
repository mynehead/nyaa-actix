use actix_session::{Session, SessionExt};
use actix_web::dev::ServiceResponse;
use actix_web::middleware::ErrorHandlerResponse;
use actix_web::{web, HttpResponse, Result};
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::utils::context::base_context;
use crate::utils::internal_error;

/// Renders one of the static info pages (rules, help).
fn info_page(page: &str, session: &Session, pool: &DbPool, tmpl: &Tera, cfg: &Config) -> Result<HttpResponse> {
    let current_user = get_current_user(session, pool);
    let mut ctx = base_context(cfg, current_user.as_ref());
    ctx.insert("active_page", page);
    let html = tmpl.render(&format!("{}.html", page), &ctx).map_err(internal_error)?;
    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

pub async fn rules(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    info_page("rules", &session, &pool, &tmpl, &cfg)
}

pub async fn help(
    session: Session,
    pool: web::Data<DbPool>,
    tmpl: web::Data<Tera>,
    cfg: web::Data<Config>,
) -> Result<HttpResponse> {
    info_page("help", &session, &pool, &tmpl, &cfg)
}

/// Swaps the body of any 404 for the site's 404 page, as upstream does with `abort(404)`.
pub fn not_found<B>(res: ServiceResponse<B>) -> Result<ErrorHandlerResponse<B>> {
    let req = res.request();
    let html = match (
        req.app_data::<web::Data<Tera>>(),
        req.app_data::<web::Data<Config>>(),
        req.app_data::<web::Data<DbPool>>(),
    ) {
        (Some(tmpl), Some(cfg), Some(pool)) => {
            let current_user = get_current_user(&req.get_session(), pool);
            tmpl.render("404.html", &base_context(cfg, current_user.as_ref())).ok()
        }
        _ => None,
    };
    let Some(html) = html else {
        return Ok(ErrorHandlerResponse::Response(res.map_into_left_body()));
    };
    let (req, res) = res.into_parts();
    let mut new_res = HttpResponse::NotFound().content_type("text/html; charset=utf-8").body(html);
    // Keep headers like Set-Cookie from the original response
    for (name, value) in res.headers() {
        if name != actix_web::http::header::CONTENT_TYPE && name != actix_web::http::header::CONTENT_LENGTH {
            new_res.headers_mut().insert(name.clone(), value.clone());
        }
    }
    Ok(ErrorHandlerResponse::Response(ServiceResponse::new(req, new_res).map_into_right_body()))
}
