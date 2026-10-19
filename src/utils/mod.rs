pub mod avatar;
pub mod context;
pub mod flash;
pub mod gravatar;
pub mod pagination;
pub mod proxy;
pub mod tera_filters;
pub mod throttle;
pub mod token;

use std::net::IpAddr;

/// For `map_err` on database, pool, storage and template errors: logs the error and
/// answers a plain 500, so its text (SQL, file paths) never reaches the visitor.
pub fn internal_error<E: std::fmt::Display>(e: E) -> actix_web::Error {
    log::error!("Internal error: {e}");
    actix_web::error::ErrorInternalServerError("Internal server error")
}

/// A 302 to `location`.
pub fn redirect(location: &str) -> actix_web::HttpResponse {
    actix_web::HttpResponse::Found().insert_header((actix_web::http::header::LOCATION, location)).finish()
}

/// A 303 to `location`: after a form post, the browser follows it with a GET.
pub fn see_other(location: &str) -> actix_web::HttpResponse {
    actix_web::HttpResponse::SeeOther().insert_header((actix_web::http::header::LOCATION, location)).finish()
}

/// Renders a template as an HTML page with `status`; template errors become [`internal_error`]s.
pub fn html_page(
    tmpl: &tera::Tera,
    name: &str,
    ctx: &tera::Context,
    status: actix_web::http::StatusCode,
) -> actix_web::Result<actix_web::HttpResponse> {
    let html = tmpl.render(name, ctx).map_err(internal_error)?;
    Ok(actix_web::HttpResponse::build(status).content_type("text/html").body(html))
}

/// Runs blocking database work on actix's thread pool, so the request threads keep serving
/// other visitors meanwhile. Pool and query errors become [`internal_error`]s.
pub async fn with_conn<T, E, F>(pool: &crate::db::DbPool, f: F) -> actix_web::Result<T>
where
    F: FnOnce(&mut crate::db::DbConnection) -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let pool = pool.clone();
    let result =
        actix_web::web::block(move || pool.get().map(|mut conn| f(&mut conn))).await.map_err(internal_error)?;
    result.map_err(internal_error)?.map_err(internal_error)
}

/// Argon2 runs in progress, for [`argon2`].
static ARGON2_BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Runs a password hash or check on actix's thread pool. Each run holds 19 MiB and a core,
/// so at most two per core run at once; past that the request gets a 503 straight away,
/// and a login flood can't use up the server's memory and CPU.
pub async fn argon2<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> actix_web::Result<T> {
    let max = 2 * std::thread::available_parallelism().map_or(1, |n| n.get());
    let Some(slot) = Slot::take(&ARGON2_BUSY, max) else {
        return Err(actix_web::error::ErrorServiceUnavailable("The server is busy. Try again in a moment."));
    };
    // The slot moves into the job, so it is freed when the hash ends, even if the visitor left
    actix_web::web::block(move || {
        let _slot = slot;
        f()
    })
    .await
    .map_err(internal_error)
}

/// One of `max` places counted in `busy`, given back on drop.
struct Slot(&'static std::sync::atomic::AtomicUsize);

impl Slot {
    fn take(busy: &'static std::sync::atomic::AtomicUsize, max: usize) -> Option<Slot> {
        use std::sync::atomic::Ordering::SeqCst;
        busy.fetch_update(SeqCst, SeqCst, |n| (n < max).then_some(n + 1)).ok().map(|_| Slot(busy))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The visitor's address: the connection's peer, or behind a proxy listed in
/// `TRUSTED_PROXIES`, the address it forwarded (see `proxy`).
pub fn client_addr(req: &actix_web::HttpRequest) -> Option<IpAddr> {
    let trusted = req.app_data::<actix_web::web::Data<crate::config::Config>>().map(|c| c.trusted_proxies.as_slice());
    proxy::resolve(req.peer_addr(), req.headers(), trusted.unwrap_or(&[]))
}

/// `client_addr` packed for the IP columns (see `pack_ip`).
pub fn client_ip(req: &actix_web::HttpRequest) -> Option<Vec<u8>> {
    client_addr(req).map(pack_ip)
}

pub fn pack_ip(addr: IpAddr) -> Vec<u8> {
    match addr {
        IpAddr::V4(v4) => {
            let mut buf = vec![0u8; 16];
            let octets = v4.octets();
            buf[12..].copy_from_slice(&octets);
            buf
        }
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

/// Reverses `pack_ip`; also reads 4-byte IPv4 values. `None` for any other length.
pub fn unpack_ip(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 if bytes[..12].iter().all(|&b| b == 0) && bytes[12..] != [0, 0, 0, 0] && bytes[12..] != [0, 0, 0, 1] => {
            Some(IpAddr::from(<[u8; 4]>::try_from(&bytes[12..]).ok()?))
        }
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => None,
    }
}

pub fn sanitize_string(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Like `sanitize_string`, but keeps line breaks and tabs, for Markdown fields
/// such as descriptions. Line endings are normalized to `\n`.
pub fn sanitize_text(s: &str) -> String {
    s.replace("\r\n", "\n").chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_run_out_and_come_back_on_drop() {
        static BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let (a, b) = (Slot::take(&BUSY, 2), Slot::take(&BUSY, 2));
        assert!(a.is_some() && b.is_some());
        assert!(Slot::take(&BUSY, 2).is_none(), "all slots are busy");
        drop(a);
        assert!(Slot::take(&BUSY, 2).is_some());
    }

    #[test]
    fn packs_ipv4_into_last_four_bytes() {
        let packed = pack_ip("192.168.1.2".parse().unwrap());
        assert_eq!(packed.len(), 16);
        assert_eq!(&packed[..12], &[0u8; 12]);
        assert_eq!(&packed[12..], &[192, 168, 1, 2]);
    }

    #[test]
    fn packs_ipv6_as_is() {
        let addr: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert_eq!(pack_ip(IpAddr::V6(addr)), addr.octets().to_vec());
        for ip in ["192.168.1.2", "2001:db8::1", "::1", "::"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert_eq!(unpack_ip(&pack_ip(ip)), Some(ip));
        }
        assert_eq!(unpack_ip(&[10, 0, 0, 1]), Some("10.0.0.1".parse().unwrap()));
        assert_eq!(unpack_ip(&[1, 2]), None);
    }

    #[test]
    fn internal_errors_hide_their_text() {
        let response = internal_error("no such table: users").error_response();
        assert_eq!(response.status(), actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = actix_web::body::to_bytes(response.into_body());
        let body = futures_util::FutureExt::now_or_never(body).unwrap().unwrap();
        assert_eq!(body, "Internal server error");
    }

    #[test]
    fn sanitize_text_keeps_line_breaks() {
        assert_eq!(sanitize_text("**a**\r\n- b\n\tc\u{0}"), "**a**\n- b\n\tc");
    }

    #[test]
    fn sanitize_removes_control_characters() {
        assert_eq!(sanitize_string("a\u{0}b\nc\td é"), "abcd é");
    }
}
