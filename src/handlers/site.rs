use actix_session::{Session, SessionExt};
use actix_web::dev::ServiceResponse;
use actix_web::http::{header, Method, StatusCode};
use actix_web::middleware::ErrorHandlerResponse;
use actix_web::{web, HttpResponse, Result};
use tera::Tera;

use crate::config::Config;
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::utils::context::base_context;
use crate::utils::html_page;
use crate::utils::redirect;

/// Renders one of the static info pages (rules, help).
fn info_page(page: &str, session: &Session, pool: &DbPool, tmpl: &Tera, cfg: &Config) -> Result<HttpResponse> {
    let current_user = get_current_user(session, pool);
    let mut ctx = base_context(cfg, current_user.as_ref());
    ctx.insert("active_page", page);
    html_page(tmpl, &format!("{}.html", page), &ctx, StatusCode::OK)
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

/// Where the 401 handler keeps the page a guest asked for, so the login can go back there.
const LOGIN_NEXT_KEY: &str = "login_next";

/// The site's error page for 401, 403 and 404 answers, as upstream does with `abort()`.
/// Every 404 gets it; a 401 or 403 only when a handler raised it as an error, so pages that
/// answer 401 themselves (the login form) and the JSON API keep their own body. A guest
/// turned away from a page (a GET answered 401) goes to the login form instead.
pub fn error_page<B>(res: ServiceResponse<B>) -> Result<ErrorHandlerResponse<B>> {
    let status = res.status();
    let message = match res.response().error() {
        _ if status == StatusCode::NOT_FOUND => "The path you requested does not exist on this server.".to_string(),
        Some(e) => e.to_string(),
        None => return Ok(ErrorHandlerResponse::Response(res.map_into_left_body())),
    };
    let req = res.request();
    if status == StatusCode::UNAUTHORIZED && req.method() == Method::GET {
        let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
        req.get_session().insert(LOGIN_NEXT_KEY, path)?;
        let (req, _) = res.into_parts();
        let new_res = redirect("/login");
        return Ok(ErrorHandlerResponse::Response(ServiceResponse::new(req, new_res).map_into_right_body()));
    }
    let html = match (
        req.app_data::<web::Data<Tera>>(),
        req.app_data::<web::Data<Config>>(),
        req.app_data::<web::Data<DbPool>>(),
    ) {
        (Some(tmpl), Some(cfg), Some(pool)) => {
            let current_user = get_current_user(&req.get_session(), pool);
            let mut ctx = base_context(cfg, current_user.as_ref());
            ctx.insert("code", &status.as_u16());
            ctx.insert("title", status.canonical_reason().unwrap_or_default());
            ctx.insert("message", &message);
            tmpl.render("error.html", &ctx).ok()
        }
        _ => None,
    };
    let Some(html) = html else {
        return Ok(ErrorHandlerResponse::Response(res.map_into_left_body()));
    };
    let (req, res) = res.into_parts();
    let mut new_res = HttpResponse::build(status).content_type("text/html; charset=utf-8").body(html);
    // Keep headers like Set-Cookie from the original response
    for (name, value) in res.headers() {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            new_res.headers_mut().insert(name.clone(), value.clone());
        }
    }
    Ok(ErrorHandlerResponse::Response(ServiceResponse::new(req, new_res).map_into_right_body()))
}

/// Where to go after logging in: the page the 401 handler sent the guest away from, or home.
pub(crate) fn take_login_next(session: &Session) -> String {
    let next: Option<String> = session.remove_as(LOGIN_NEXT_KEY).and_then(|r| r.ok());
    // Only paths on this site: "//host" and "/\host" would leave it
    next.filter(|p| p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\")).unwrap_or_else(|| "/".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::cookie::{Cookie, Key};
    use actix_web::middleware::ErrorHandlers;
    use actix_web::{test, App};

    use crate::auth::LoggedIn;

    async fn body<B: actix_web::body::MessageBody>(res: ServiceResponse<B>) -> String {
        String::from_utf8(test::read_body(res).await.to_vec()).unwrap()
    }

    #[actix_web::test]
    async fn error_pages_and_login_return() {
        let pool: DbPool =
            diesel::r2d2::Pool::builder().max_size(1).build(crate::db::DbManager::new(&crate::db::test_url())).unwrap();
        crate::db::run_migrations(&mut pool.get().unwrap()).unwrap();
        let mut tera = Tera::new("templates/**/*").unwrap();
        crate::utils::tera_filters::register(&mut tera);
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Config::for_tests()))
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(tera))
                .wrap(
                    ErrorHandlers::new()
                        .handler(StatusCode::UNAUTHORIZED, error_page)
                        .handler(StatusCode::FORBIDDEN, error_page)
                        .handler(StatusCode::NOT_FOUND, error_page),
                )
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .route("/private", web::route().to(|_: LoggedIn| async { HttpResponse::Ok().finish() }))
                .route(
                    "/forbidden",
                    web::get().to(|| async { Err::<HttpResponse, _>(actix_web::error::ErrorForbidden("You may not")) }),
                )
                .route("/own", web::get().to(|| async { HttpResponse::Unauthorized().body("own page") }))
                .route(
                    "/plant",
                    web::get().to(|s: Session| async move {
                        s.insert(LOGIN_NEXT_KEY, "//evil.example").map(|_| HttpResponse::Ok())
                    }),
                )
                .route("/next", web::get().to(|s: Session| async move { take_login_next(&s) })),
        )
        .await;
        let get = |uri: &str, cookie: Option<&Cookie<'static>>| {
            let req = test::TestRequest::get().uri(uri);
            match cookie {
                Some(c) => req.cookie(c.clone()).to_request(),
                None => req.to_request(),
            }
        };

        // A guest's GET goes to the login form, which then returns to the page once
        let res = test::call_service(&app, get("/private?p=2", None)).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(res.headers().get(header::LOCATION).unwrap(), "/login");
        let cookie = res.response().cookies().next().unwrap().into_owned();
        let res = test::call_service(&app, get("/next", Some(&cookie))).await;
        let cookie = res.response().cookies().next().unwrap().into_owned();
        assert_eq!(body(res).await, "/private?p=2");
        assert_eq!(body(test::call_service(&app, get("/next", Some(&cookie))).await).await, "/");
        // Never off the site
        let res = test::call_service(&app, get("/plant", None)).await;
        let cookie = res.response().cookies().next().unwrap().into_owned();
        assert_eq!(body(test::call_service(&app, get("/next", Some(&cookie))).await).await, "/");

        // A guest's POST, a 403 and a 404 get the error page
        let res = test::call_service(&app, test::TestRequest::post().uri("/private").to_request()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let page = body(res).await;
        assert!(page.contains("401 Unauthorized") && page.contains("Login required"), "{page}");
        let res = test::call_service(&app, get("/forbidden", None)).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let page = body(res).await;
        assert!(page.contains("403 Forbidden") && page.contains("You may not") && page.contains("</html>"), "{page}");
        let res = test::call_service(&app, get("/nothing", None)).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(body(res).await.contains("404 Not Found"));
        // A page that answers 401 itself keeps its body
        let res = test::call_service(&app, get("/own", None)).await;
        assert_eq!((res.status(), body(res).await.as_str()), (StatusCode::UNAUTHORIZED, "own page"));
    }
}
