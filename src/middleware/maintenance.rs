//! Upstream's maintenance mode (MAINTENANCE_MODE): the site stays readable, but every request
//! that would change something is turned back. Browsers go back to the page they came from
//! with the maintenance message as a flash; the API answers 503 with it as the error.
//! Logging in and out still work while MAINTENANCE_MODE_LOGINS is on.
//!
//! MAINTENANCE_MODE_OFFLINE goes further: everyone below moderator gets static/maintenance.html
//! as a 503 with Retry-After (the API and feeds the same status), so staff can look the site
//! over before it opens again. The tracker is a separate service and keeps answering announces.

use actix_session::SessionExt;
use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpResponse};

use crate::auth::Permission;
use crate::config::{Config, MaintenanceConfig};
use crate::db::DbPool;
use crate::middleware::auth::get_current_user;
use crate::utils::flash;

/// The page a reverse proxy also serves while the site is stopped (see docker/DEPLOY.md).
const PAGE: &str = include_str!("../../static/maintenance.html");
/// The page's own notice, replaced by MAINTENANCE_MODE_MESSAGE when that is set.
const PAGE_MESSAGE: &str = "We are doing some maintenance and will be back shortly.";

pub async fn read_only(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    let maintenance = req.app_data::<web::Data<Config>>().map(|cfg| &cfg.maintenance).filter(|m| m.enabled);
    if let Some(m) = maintenance {
        if m.offline && !reachable_offline(m, req.path()) && !is_staff(&req) {
            let response = offline(&req, m);
            return Ok(req.into_response(response));
        }
        if !allowed(m, req.method(), req.path()) {
            let response = refuse(&req, m);
            return Ok(req.into_response(response));
        }
    }
    next.call(req).await.map(ServiceResponse::map_into_boxed_body)
}

fn allowed(m: &MaintenanceConfig, method: &Method, path: &str) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || path == "/logout"
        // The two-factor step is part of logging in
        || (m.logins && (path == "/login" || path == "/login/2fa"))
}

/// What guests may still open while the site is offline: enough for staff to log in.
fn reachable_offline(m: &MaintenanceConfig, path: &str) -> bool {
    path == "/logout"
        || path.starts_with("/static/")
        || (m.logins && (path == "/login" || path == "/login/2fa" || path == "/captcha/challenge"))
}

fn is_staff(req: &ServiceRequest) -> bool {
    let Some(pool) = req.app_data::<web::Data<DbPool>>() else { return false };
    get_current_user(&req.get_session(), pool).is_some_and(|u| u.can(Permission::ViewAdminPages))
}

fn offline(req: &ServiceRequest, m: &MaintenanceConfig) -> HttpResponse {
    let mut res = HttpResponse::ServiceUnavailable();
    res.insert_header((header::RETRY_AFTER, m.retry_after)).insert_header((header::CACHE_CONTROL, "no-store"));
    if req.path().starts_with("/api/") {
        return res.json(serde_json::json!({ "errors": [m.message] }));
    }
    // The read-only default ("…read-only maintenance mode.") would be wrong here; the page
    // has its own wording unless MAINTENANCE_MODE_MESSAGE was set
    let page = if m.message == MaintenanceConfig::default().message {
        PAGE.to_string()
    } else {
        PAGE.replacen(PAGE_MESSAGE, &tera::escape_html(&m.message), 1)
    };
    res.content_type("text/html; charset=utf-8").body(page)
}

fn refuse(req: &ServiceRequest, m: &MaintenanceConfig) -> HttpResponse {
    if req.path().starts_with("/api/") {
        return HttpResponse::ServiceUnavailable()
            .insert_header((header::RETRY_AFTER, m.retry_after))
            .json(serde_json::json!({ "errors": [m.message] }));
    }
    let what = if req.path().starts_with("/login") { "Logging in is disabled during maintenance." } else { "" };
    flash::push(&req.get_session(), "danger", &m.message, what);
    // Back to the page the form was on; only its path is kept, so this never leads off-site
    let back = req
        .headers()
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.find("://").and_then(|i| r[i + 3..].find('/').map(|j| r[i + 3 + j..].to_string())))
        // `//host` and `/\host` would be read as another site
        .filter(|path| !path[1..].starts_with(['/', '\\']))
        .unwrap_or_else(|| req.path().to_string());
    HttpResponse::SeeOther().insert_header((header::LOCATION, back)).finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{storage::CookieSessionStore, SessionMiddleware};
    use actix_web::cookie::Key;
    use actix_web::http::StatusCode;
    use actix_web::{test, App};

    async fn call(maintenance: MaintenanceConfig, req: test::TestRequest) -> (StatusCode, String, String) {
        let cfg = Config { maintenance, ..Config::for_tests() };
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(cfg))
                .wrap(actix_web::middleware::from_fn(read_only))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .default_service(web::to(|| async { HttpResponse::Ok().body("done") })),
        )
        .await;
        let res = test::call_service(&app, req.to_request()).await;
        let status = res.status();
        let location = res.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
        (status, location, String::from_utf8(test::read_body(res).await.to_vec()).unwrap())
    }

    fn on() -> MaintenanceConfig {
        MaintenanceConfig { enabled: true, ..Default::default() }
    }

    #[actix_web::test]
    async fn off_by_default() {
        let (status, _, body) = call(Default::default(), test::TestRequest::post().uri("/upload")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "done"));
    }

    #[actix_web::test]
    async fn pages_still_show_but_posts_go_back() {
        let (status, _, _) = call(on(), test::TestRequest::get().uri("/upload")).await;
        assert_eq!(status, StatusCode::OK);
        let (status, location, _) = call(
            on(),
            test::TestRequest::post().uri("/view/5").insert_header((header::REFERER, "http://nyaa.test/view/5?x=1")),
        )
        .await;
        assert_eq!((status, location.as_str()), (StatusCode::SEE_OTHER, "/view/5?x=1"));
        let (_, location, _) = call(
            on(),
            test::TestRequest::post().uri("/view/5").insert_header((header::REFERER, "http://nyaa.test//evil.test/")),
        )
        .await;
        assert_eq!(location, "/view/5");
        let (status, location, _) = call(on(), test::TestRequest::post().uri("/register")).await;
        assert_eq!((status, location.as_str()), (StatusCode::SEE_OTHER, "/register"));
    }

    #[actix_web::test]
    async fn logins_follow_their_own_switch() {
        for path in ["/login", "/login/2fa", "/logout"] {
            let (status, _, _) = call(on(), test::TestRequest::post().uri(path)).await;
            assert_eq!(status, StatusCode::OK, "{path}");
        }
        let no_logins = MaintenanceConfig { logins: false, ..on() };
        for path in ["/login", "/login/2fa"] {
            let (status, _, _) = call(no_logins.clone(), test::TestRequest::post().uri(path)).await;
            assert_eq!(status, StatusCode::SEE_OTHER, "{path}");
        }
        let (status, _, _) = call(no_logins, test::TestRequest::post().uri("/logout")).await;
        assert_eq!(status, StatusCode::OK);
    }

    fn offline() -> MaintenanceConfig {
        MaintenanceConfig { offline: true, ..on() }
    }

    #[actix_web::test]
    async fn offline_shows_the_maintenance_page_to_guests() {
        for req in [
            test::TestRequest::get().uri("/"),
            test::TestRequest::get().uri("/view/5/torrent"),
            test::TestRequest::post().uri("/upload"),
        ] {
            let (status, _, body) = call(offline(), req).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(body.contains(PAGE_MESSAGE), "{body}");
        }
        let (status, _, body) = call(offline(), test::TestRequest::get().uri("/api/info/5")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.starts_with(r#"{"errors":"#), "{body}");
    }

    #[actix_web::test]
    async fn offline_page_carries_retry_after_and_the_message() {
        let m = MaintenanceConfig { message: "Back at <b>10</b> UTC".into(), retry_after: 900, ..offline() };
        let cfg = Config { maintenance: m, ..Config::for_tests() };
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(cfg))
                .wrap(actix_web::middleware::from_fn(read_only))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                .default_service(web::to(|| async { HttpResponse::Ok().body("done") })),
        )
        .await;
        let res = test::call_service(&app, test::TestRequest::get().uri("/").to_request()).await;
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "900");
        let body = String::from_utf8(test::read_body(res).await.to_vec()).unwrap();
        assert!(body.contains("Back at &lt;b&gt;10&lt;&#x2F;b&gt; UTC"), "{body}");
        assert!(!body.contains(PAGE_MESSAGE));
    }

    #[actix_web::test]
    async fn offline_still_lets_staff_log_in() {
        for path in ["/login", "/login/2fa", "/captcha/challenge", "/static/css/main.css"] {
            let (status, _, _) = call(offline(), test::TestRequest::get().uri(path)).await;
            assert_eq!(status, StatusCode::OK, "{path}");
        }
        let (status, _, _) = call(offline(), test::TestRequest::post().uri("/login")).await;
        assert_eq!(status, StatusCode::OK);
        let no_logins = MaintenanceConfig { logins: false, ..offline() };
        let (status, _, _) = call(no_logins, test::TestRequest::get().uri("/login")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[actix_web::test]
    async fn offline_lets_moderators_through_but_not_users() {
        use crate::middleware::auth::test_support::login;
        use diesel::r2d2::Pool;
        use diesel::RunQueryDsl;

        // One connection, so every request sees the same in-memory database
        let pool = Pool::builder().max_size(1).build(crate::db::DbManager::new(":memory:")).unwrap();
        crate::db::run_migrations(&mut pool.get().unwrap()).unwrap();
        diesel::sql_query(
            "INSERT INTO users (id, username, password_hash, status, level) VALUES \
                           (1, 'user', 'x', 1, 0), (3, 'mod', 'x', 1, 2)",
        )
        .execute(&mut pool.get().unwrap())
        .unwrap();
        let cfg = Config { maintenance: offline(), ..Config::for_tests() };
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(cfg))
                .app_data(web::Data::new(pool))
                .wrap(actix_web::middleware::from_fn(read_only))
                .wrap(SessionMiddleware::new(CookieSessionStore::default(), Key::from(&[7u8; 64])))
                // Under /static/, which stays reachable while offline
                .route("/static/test-login/{id}", web::get().to(login))
                .default_service(web::to(|| async { HttpResponse::Ok().body("done") })),
        )
        .await;
        for (id, expected) in [(1, StatusCode::SERVICE_UNAVAILABLE), (3, StatusCode::OK)] {
            let login_req = test::TestRequest::get().uri(&format!("/static/test-login/{id}"));
            let res = test::call_service(&app, login_req.to_request()).await;
            let cookie = res.response().cookies().next().unwrap().into_owned();
            let res = test::call_service(&app, test::TestRequest::get().uri("/").cookie(cookie).to_request()).await;
            assert_eq!(res.status(), expected, "user {id}");
        }
    }

    #[actix_web::test]
    async fn api_gets_503_with_the_message() {
        let m = MaintenanceConfig { message: "Back at 10 UTC".into(), ..on() };
        let (status, _, body) = call(m, test::TestRequest::post().uri("/api/upload")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, r#"{"errors":["Back at 10 UTC"]}"#);
    }
}
