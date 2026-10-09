//! Cross-site request forgery guard. Every request that can change something (anything
//! but GET, HEAD and OPTIONS) must come from a page on this site. Browsers send `Origin`
//! on every POST, and a cross-site page can't forge it; `Referer` is the fallback for the
//! rare browser that leaves `Origin` out. A request with neither is refused, so a form
//! post only works from the site itself.
//!
//! "This site" is the host the request was sent to, or the host in `SITE_URL` (for a
//! reverse proxy that rewrites `Host`).
//!
//! `/api/` is left out: scripts call it without `Origin`, and it signs in with HTTP Basic
//! auth only, never the session cookie, so a cross-site page has nothing to ride on.

use actix_web::body::{BoxBody, MessageBody};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::{header, Method};
use actix_web::middleware::Next;
use actix_web::{web, Error, HttpResponse};

use crate::config::Config;

pub async fn reject_cross_site(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<BoxBody>, Error> {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) || req.path().starts_with("/api/");
    if !safe && !from_this_site(&req) {
        log::warn!("Refused cross-site {} {} (origin {:?})", req.method(), req.path(), source(&req));
        let response = HttpResponse::Forbidden().body("This request didn't come from this site.");
        return Ok(req.into_response(response));
    }
    next.call(req).await.map(ServiceResponse::map_into_boxed_body)
}

/// The page that sent the request: `Origin`, else `Referer`.
fn source(req: &ServiceRequest) -> Option<&str> {
    let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
    // Privacy-sensitive contexts send the literal origin "null"; that never matches
    header(header::ORIGIN).or_else(|| header(header::REFERER))
}

fn from_this_site(req: &ServiceRequest) -> bool {
    let Some(source) = source(req).and_then(authority) else {
        return false;
    };
    let host = req.connection_info().host().to_ascii_lowercase();
    let site = req.app_data::<web::Data<Config>>().and_then(|cfg| authority(&cfg.site_url));
    source == host || Some(source) == site
}

/// `host[:port]` of an absolute http(s) URL, lowercased.
fn authority(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    // Userinfo (`user@host`) never appears in a browser's Origin; refuse it rather than parse it
    (!authority.is_empty() && !authority.contains('@')).then(|| authority.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as atest, App};

    #[test]
    fn authority_of_urls() {
        assert_eq!(authority("https://Nyaa.example/view/1").as_deref(), Some("nyaa.example"));
        assert_eq!(authority("http://localhost:8080").as_deref(), Some("localhost:8080"));
        assert_eq!(authority("http://localhost:8080?x").as_deref(), Some("localhost:8080"));
        assert_eq!(authority("null"), None);
        assert_eq!(authority("https://"), None);
        assert_eq!(authority("https://me@evil.example/"), None);
        assert_eq!(authority("javascript:alert(1)"), None);
    }

    async fn status(site_url: &str, req: atest::TestRequest) -> u16 {
        let cfg = Config {
            database_url: String::new(),
            secret_key: String::new(),
            site_name: "Nyaa".into(),
            site_flavor: "nyaa".into(),
            results_per_page: 75,
            max_pages: 0,
            torrent_storage_path: String::new(),
            avatar_storage_path: String::new(),
            enable_gravatar: false,
            maintenance_mode: false,
            site_url: site_url.into(),
            tracker_urls: vec![],
            trusted_proxies: vec![],
            meili: None,
            tracker: None,
            ratelimit_account_age: 0,
            editing_time_limit: 0,
            trusted: Default::default(),
            tickets: Default::default(),
        };
        let app = atest::init_service(
            App::new()
                .app_data(web::Data::new(cfg))
                .wrap(actix_web::middleware::from_fn(reject_cross_site))
                .default_service(web::to(HttpResponse::Ok)),
        )
        .await;
        atest::call_service(&app, req.insert_header((header::HOST, "nyaa.example")).to_request())
            .await
            .status()
            .as_u16()
    }

    #[actix_web::test]
    async fn same_site_posts_pass() {
        let post = || atest::TestRequest::post().uri("/upload");
        assert_eq!(status("", post().insert_header((header::ORIGIN, "https://nyaa.example"))).await, 200);
        assert_eq!(status("", post().insert_header((header::REFERER, "https://nyaa.example/upload"))).await, 200);
        // Behind a proxy that rewrites Host, SITE_URL names the site
        assert_eq!(
            status("https://public.example", post().insert_header((header::ORIGIN, "https://public.example"))).await,
            200
        );
    }

    #[actix_web::test]
    async fn cross_site_and_unsourced_posts_are_refused() {
        let post = || atest::TestRequest::post().uri("/upload");
        assert_eq!(status("", post().insert_header((header::ORIGIN, "https://evil.example"))).await, 403);
        assert_eq!(status("", post().insert_header((header::ORIGIN, "null"))).await, 403);
        assert_eq!(
            status("", post().insert_header((header::REFERER, "https://nyaa.example.evil.example/"))).await,
            403
        );
        // Origin wins over a matching Referer
        assert_eq!(
            status(
                "",
                post()
                    .insert_header((header::ORIGIN, "https://evil.example"))
                    .insert_header((header::REFERER, "https://nyaa.example/"))
            )
            .await,
            403
        );
        assert_eq!(status("", post()).await, 403);
    }

    #[actix_web::test]
    async fn api_posts_need_no_origin() {
        assert_eq!(status("", atest::TestRequest::post().uri("/api/upload")).await, 200);
        assert_eq!(status("", atest::TestRequest::post().uri("/apiary")).await, 403);
    }

    #[actix_web::test]
    async fn reads_are_never_checked() {
        assert_eq!(
            status("", atest::TestRequest::get().uri("/").insert_header((header::ORIGIN, "https://evil.example")))
                .await,
            200
        );
        assert_eq!(status("", atest::TestRequest::get().uri("/")).await, 200);
    }
}
