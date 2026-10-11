//! The request log (actix's `Logger`), with secrets in URLs masked.

use actix_web::{dev::ServiceRequest, http::header::REFERER, middleware::Logger};
use regex::Regex;
use std::sync::LazyLock;

/// Password-reset and activation tokens in the path, invite codes in the query.
static SECRETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(/password-reset/|/user/activate/|[?&]invite=)[^/?&#\s]+").unwrap());

pub fn redact(s: &str) -> String {
    SECRETS.replace_all(s, "${1}***").into_owned()
}

/// Same fields as `Logger::default()`, with the request line and referer redacted.
pub fn logger() -> Logger {
    Logger::new(r#"%a "%{request}xi" %s %b "%{referer}xi" "%{User-Agent}i" %T"#)
        .custom_request_replace("request", |req: &ServiceRequest| {
            format!("{} {} {:?}", req.method(), redact(&req.uri().to_string()), req.version())
        })
        .custom_request_replace("referer", |req: &ServiceRequest| {
            req.headers().get(REFERER).and_then(|v| v.to_str().ok()).map_or("-".into(), redact)
        })
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn masks_tokens_and_invite_codes() {
        assert_eq!(redact("/password-reset/abc.def?x=1"), "/password-reset/***?x=1");
        assert_eq!(redact("https://site/user/activate/tok"), "https://site/user/activate/***");
        assert_eq!(redact("/register?a=1&invite=CODE&b=2"), "/register?a=1&invite=***&b=2");
        assert_eq!(redact("/register?invite=CODE"), "/register?invite=***");
        assert_eq!(redact("/?q=invite=x&c=1_0"), "/?q=invite=x&c=1_0");
        assert_eq!(redact("/view/12"), "/view/12");
    }
}
