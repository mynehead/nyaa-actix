//! Upstream's maintenance mode (MAINTENANCE_MODE): the site stays readable, but every request
//! that would change something is turned back. Browsers go back to the page they came from
//! with the maintenance message as a flash; the API answers 503 with it as the error.
//! Logging in and out still work while MAINTENANCE_MODE_LOGINS is on.

use actix_session::SessionExt;
use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpResponse};

use crate::config::{Config, MaintenanceConfig};
use crate::utils::flash;

pub async fn read_only(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    let maintenance = req.app_data::<web::Data<Config>>().map(|cfg| &cfg.maintenance).filter(|m| m.enabled);
    if let Some(m) = maintenance {
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
        || (m.logins && path == "/login")
}

fn refuse(req: &ServiceRequest, m: &MaintenanceConfig) -> HttpResponse {
    if req.path().starts_with("/api/") {
        return HttpResponse::ServiceUnavailable().json(serde_json::json!({ "errors": [m.message] }));
    }
    let what = if req.path() == "/login" { "Logging in is disabled during maintenance." } else { "" };
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
        for path in ["/login", "/logout"] {
            let (status, _, _) = call(on(), test::TestRequest::post().uri(path)).await;
            assert_eq!(status, StatusCode::OK, "{path}");
        }
        let no_logins = MaintenanceConfig { logins: false, ..on() };
        let (status, _, _) = call(no_logins.clone(), test::TestRequest::post().uri("/login")).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, _, _) = call(no_logins, test::TestRequest::post().uri("/logout")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[actix_web::test]
    async fn api_gets_503_with_the_message() {
        let m = MaintenanceConfig { message: "Back at 10 UTC".into(), ..on() };
        let (status, _, body) = call(m, test::TestRequest::post().uri("/api/upload")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, r#"{"errors":["Back at 10 UTC"]}"#);
    }
}
