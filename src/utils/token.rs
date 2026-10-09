//! Signed, tamper-proof tokens for links sent by mail (account activation, password reset),
//! like upstream's itsdangerous serializer: base64url JSON fields, a dot, and an
//! HMAC-SHA256 of them keyed with SECRET_KEY. Each token names its purpose, so one kind
//! can't be used as another.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn mac(secret: &str, payload: &str) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes keys of any length");
    mac.update(payload.as_bytes());
    mac
}

/// A token carrying `purpose` and `fields`.
pub fn sign(secret: &str, purpose: &str, fields: &[serde_json::Value]) -> String {
    let mut all = vec![serde_json::Value::from(purpose)];
    all.extend_from_slice(fields);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::Value::Array(all).to_string());
    let signature = URL_SAFE_NO_PAD.encode(mac(secret, &payload).finalize().into_bytes());
    format!("{payload}.{signature}")
}

/// The fields of a token for `purpose`, if it was signed with `secret`.
pub fn verify(secret: &str, purpose: &str, token: &str) -> Option<Vec<serde_json::Value>> {
    let (payload, signature) = token.split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    mac(secret, payload).verify_slice(&signature).ok()?;
    let json = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let mut fields = match serde_json::from_slice(&json).ok()? {
        serde_json::Value::Array(fields) => fields,
        _ => return None,
    };
    (fields.first()?.as_str()? == purpose).then(|| fields.split_off(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_and_tampering() {
        let token = sign("secret", "activate", &[json!(42)]);
        assert!(!token.contains(['/', '+', '=']), "fits in a URL path: {token}");
        assert_eq!(verify("secret", "activate", &token), Some(vec![json!(42)]));
        assert_eq!(verify("other secret", "activate", &token), None);
        assert_eq!(verify("secret", "reset", &token), None, "purpose is part of the token");
        let (payload, signature) = token.split_once('.').unwrap();
        let forged = URL_SAFE_NO_PAD.encode(r#"["activate",1]"#);
        assert_eq!(verify("secret", "activate", &format!("{forged}.{signature}")), None);
        assert_eq!(verify("secret", "activate", payload), None);
        assert_eq!(verify("secret", "activate", "not a token"), None);
    }
}
