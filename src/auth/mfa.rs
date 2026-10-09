//! Two-factor sign-in for password logins: TOTP codes (RFC 6238: SHA-1, 6 digits, 30 s,
//! what every authenticator app speaks) plus ten one-time recovery codes.
//!
//! The TOTP secret is stored encrypted (ChaCha20-Poly1305, key derived from SECRET_KEY with
//! HKDF, the user id as associated data), so a database dump alone can't generate codes.
//! Changing SECRET_KEY therefore makes every stored secret unreadable; those users sign in
//! with a recovery code, or an admin resets their two-factor.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use chrono::NaiveDateTime;
use diesel::prelude::*;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use totp_rs::{Algorithm, Builder, Secret, Totp};

use crate::db::schema::{user_mfa, user_recovery_codes};
use crate::db::DbConnection;
use crate::models::UserLevel;

pub const RECOVERY_CODE_COUNT: usize = 10;
/// When this few recovery codes are left, signing in with one says so.
pub const RECOVERY_CODES_LOW: i64 = 2;
const STEP_SECS: u64 = 30;
const NONCE_LEN: usize = 12;

/// Two-factor settings from the environment.
#[derive(Clone, Debug, Default)]
pub struct MfaConfig {
    /// MFA_REQUIRED_LEVEL: users of this level and up must set up two-factor before they
    /// can use the site; None (the default) leaves it up to each user.
    pub required_level: Option<UserLevel>,
    /// MFA_ISSUER_NAME: the name authenticator apps show for the account (default: SITE_NAME).
    pub issuer: String,
}

impl MfaConfig {
    pub fn from_env(site_name: &str) -> Self {
        let level = std::env::var("MFA_REQUIRED_LEVEL").unwrap_or_default();
        MfaConfig {
            required_level: parse_required_level(&level).unwrap_or_else(|| {
                panic!("MFA_REQUIRED_LEVEL must be none, trusted, moderator or admin, not `{level}`")
            }),
            issuer: std::env::var("MFA_ISSUER_NAME").ok().filter(|s| !s.trim().is_empty()).unwrap_or(site_name.into()),
        }
    }

    /// Whether `level` must have two-factor set up.
    pub fn required_for(&self, level: UserLevel) -> bool {
        self.required_level.is_some_and(|required| level >= required)
    }
}

/// `none`/empty → Some(None); a level name → Some(Some(level)); anything else → None.
fn parse_required_level(v: &str) -> Option<Option<UserLevel>> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "off" => Some(None),
        "trusted" => Some(Some(UserLevel::Trusted)),
        "moderator" | "mod" => Some(Some(UserLevel::Moderator)),
        "admin" | "superadmin" => Some(Some(UserLevel::SuperAdmin)),
        _ => None,
    }
}

/// A fresh 160-bit TOTP secret, as RFC 4226 recommends.
pub fn new_secret() -> Vec<u8> {
    let mut bytes = vec![0u8; 20];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// The secret as the Base32 text people type into an app that can't scan the QR code.
pub fn secret_base32(secret: &[u8]) -> String {
    Secret::new(secret.into()).to_base32()
}

pub fn secret_from_base32(text: &str) -> Option<Vec<u8>> {
    Secret::try_from_base32(text).ok().map(|s| s.as_bytes().to_vec())
}

/// The TOTP generator for `secret`; issuer and account name only matter for the QR code.
fn totp(secret: &[u8], issuer: &str, account: &str) -> Totp {
    // Neither may contain a colon (it separates them in the otpauth label)
    let clean = |s: &str| s.replace(':', " ");
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(STEP_SECS)
        .with_secret(secret.to_vec())
        .with_issuer(Some(clean(issuer)))
        .with_account_name(clean(account))
        .build_noncompliant()
}

/// The `otpauth://` URL the QR code carries.
pub fn otpauth_url(secret: &[u8], issuer: &str, account: &str) -> String {
    totp(secret, issuer, account).to_url().unwrap_or_default()
}

/// The QR code of `url` as an inline SVG.
pub fn qr_svg(url: &str) -> String {
    match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(200, 200)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build(),
        Err(_) => String::new(),
    }
}

pub(crate) fn unix_now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// The 30-second step `code` is valid for at `time` (one step of clock drift either way),
/// or None. Spaces in the code are ignored.
pub fn check_code(secret: &[u8], code: &str, time: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    totp(secret, "", "").check(&code, time)
}

/// The code an authenticator app shows for `secret` at `time`.
#[cfg(test)]
pub fn code_at(secret: &[u8], time: u64) -> String {
    totp(secret, "", "").generate(time).to_string()
}

pub fn check_code_now(secret: &[u8], code: &str) -> Option<u64> {
    check_code(secret, code, unix_now())
}

fn cipher(secret_key: &str) -> ChaCha20Poly1305 {
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(None, secret_key.as_bytes())
        .expand(b"nyaa-actix totp secret v1", &mut key)
        .expect("32 bytes is a valid HKDF output length");
    ChaCha20Poly1305::new(&key.into())
}

/// `secret` encrypted for storage: nonce followed by ciphertext.
pub fn encrypt_secret(secret_key: &str, user_id: i32, secret: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let aad = user_id.to_be_bytes();
    let sealed = cipher(secret_key)
        .encrypt(&Nonce::from(nonce), Payload { msg: secret, aad: &aad })
        .expect("encrypting a short secret cannot fail");
    [nonce.as_slice(), &sealed].concat()
}

/// The stored secret, or None when it was encrypted with another SECRET_KEY (or for another user).
pub fn decrypt_secret(secret_key: &str, user_id: i32, stored: &[u8]) -> Option<Vec<u8>> {
    if stored.len() <= NONCE_LEN {
        return None;
    }
    let (nonce, sealed) = stored.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
    let aad = user_id.to_be_bytes();
    cipher(secret_key).decrypt(&Nonce::from(nonce), Payload { msg: sealed, aad: &aad }).ok()
}

/// Ten new recovery codes like `k3m9q-x2p7w` (50 random bits each), for showing once.
pub fn new_recovery_codes() -> Vec<String> {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            let mut bytes = [0u8; 10];
            OsRng.fill_bytes(&mut bytes);
            let chars: String = bytes.iter().map(|b| ALPHABET[(*b & 31) as usize] as char).collect();
            format!("{}-{}", &chars[..5], &chars[5..])
        })
        .collect()
}

/// The stored form of a recovery code; case, spaces and dashes don't matter.
pub fn hash_recovery_code(code: &str) -> String {
    let normalized: String =
        code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

/// A user's two-factor setup (`user_mfa` row).
#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = user_mfa)]
pub struct UserMfa {
    pub user_id: i32,
    pub totp_secret: Vec<u8>,
    pub enabled_time: NaiveDateTime,
}

impl UserMfa {
    pub fn get(conn: &mut DbConnection, user_id: i32) -> QueryResult<Option<UserMfa>> {
        user_mfa::table.find(user_id).select(UserMfa::as_select()).first(conn).optional()
    }

    pub fn is_enabled(conn: &mut DbConnection, user_id: i32) -> QueryResult<bool> {
        let n: i64 = user_mfa::table.find(user_id).count().get_result(conn)?;
        Ok(n > 0)
    }

    /// Turns two-factor on (or replaces it) with an encrypted secret whose code for `step`
    /// was just entered, and the hashes of fresh recovery codes.
    pub fn enable(
        conn: &mut DbConnection,
        user_id: i32,
        encrypted_secret: &[u8],
        step: u64,
        code_hashes: &[String],
    ) -> QueryResult<()> {
        conn.transaction(|conn| {
            diesel::delete(user_mfa::table.find(user_id)).execute(conn)?;
            diesel::insert_into(user_mfa::table)
                .values((
                    user_mfa::user_id.eq(user_id),
                    user_mfa::totp_secret.eq(encrypted_secret),
                    user_mfa::enabled_time.eq(chrono::Utc::now().naive_utc()),
                    user_mfa::last_used_step.eq(step as i64),
                ))
                .execute(conn)?;
            Self::store_recovery_codes(conn, user_id, code_hashes)
        })
    }

    /// Replaces the user's recovery codes.
    pub fn store_recovery_codes(conn: &mut DbConnection, user_id: i32, code_hashes: &[String]) -> QueryResult<()> {
        conn.transaction(|conn| {
            diesel::delete(user_recovery_codes::table.filter(user_recovery_codes::user_id.eq(user_id)))
                .execute(conn)?;
            for hash in code_hashes {
                diesel::insert_into(user_recovery_codes::table)
                    .values((user_recovery_codes::user_id.eq(user_id), user_recovery_codes::code_hash.eq(hash)))
                    .execute(conn)?;
            }
            Ok(())
        })
    }

    /// Turns two-factor off; whether it was on.
    pub fn disable(conn: &mut DbConnection, user_id: i32) -> QueryResult<bool> {
        conn.transaction(|conn| {
            diesel::delete(user_recovery_codes::table.filter(user_recovery_codes::user_id.eq(user_id)))
                .execute(conn)?;
            Ok(diesel::delete(user_mfa::table.find(user_id)).execute(conn)? > 0)
        })
    }

    pub fn recovery_codes_left(conn: &mut DbConnection, user_id: i32) -> QueryResult<i64> {
        user_recovery_codes::table
            .filter(user_recovery_codes::user_id.eq(user_id))
            .filter(user_recovery_codes::used_time.is_null())
            .count()
            .get_result(conn)
    }

    /// Records that a code for `step` was accepted. False when that step (or a later one)
    /// was already used, so a code seen over someone's shoulder can't be replayed.
    fn claim_step(conn: &mut DbConnection, user_id: i32, step: u64) -> QueryResult<bool> {
        let n = diesel::update(user_mfa::table.find(user_id).filter(user_mfa::last_used_step.lt(step as i64)))
            .set(user_mfa::last_used_step.eq(step as i64))
            .execute(conn)?;
        Ok(n > 0)
    }

    /// Uses up the recovery code; false when it isn't one of the user's unused codes.
    fn claim_recovery_code(conn: &mut DbConnection, user_id: i32, code: &str) -> QueryResult<bool> {
        let n = diesel::update(
            user_recovery_codes::table
                .filter(user_recovery_codes::user_id.eq(user_id))
                .filter(user_recovery_codes::code_hash.eq(hash_recovery_code(code)))
                .filter(user_recovery_codes::used_time.is_null()),
        )
        .set(user_recovery_codes::used_time.eq(chrono::Utc::now().naive_utc()))
        .execute(conn)?;
        Ok(n > 0)
    }

    /// Checks a code typed at sign-in (or to change two-factor settings) and uses it up:
    /// six digits are a TOTP code, anything else a recovery code.
    pub fn verify(&self, conn: &mut DbConnection, secret_key: &str, code: &str) -> QueryResult<Option<SecondFactor>> {
        let code = code.trim();
        if code.is_empty() {
            return Ok(None);
        }
        let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit()) {
            let Some(secret) = decrypt_secret(secret_key, self.user_id, &self.totp_secret) else {
                log::warn!("Two-factor secret of user {} can't be decrypted (SECRET_KEY changed?)", self.user_id);
                return Ok(None);
            };
            return Ok(match check_code_now(&secret, code) {
                Some(step) if Self::claim_step(conn, self.user_id, step)? => Some(SecondFactor::Totp),
                _ => None,
            });
        }
        Ok(Self::claim_recovery_code(conn, self.user_id, code)?.then_some(SecondFactor::RecoveryCode))
    }
}

/// Which second factor a code was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondFactor {
    Totp,
    RecoveryCode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_test_vector() {
        // RFC 6238 appendix B, SHA-1 secret "12345678901234567890", last 6 of the 8 digits
        let secret = b"12345678901234567890";
        assert_eq!(check_code(secret, "287082", 59), Some(1));
        assert_eq!(check_code(secret, "081804", 1111111109), Some(1111111109 / 30));
        // One step of drift either way, not two
        assert_eq!(check_code(secret, "081804", 1111111109 + 30), Some(1111111109 / 30));
        assert_eq!(check_code(secret, "081804", 1111111109 + 60), None);
        assert_eq!(check_code(secret, "081 804", 1111111109), Some(1111111109 / 30));
        assert_eq!(check_code(secret, "81804", 1111111109), None);
    }

    #[test]
    fn secret_round_trips_only_with_the_same_key_and_user() {
        let secret = new_secret();
        let stored = encrypt_secret("key one", 7, &secret);
        assert_ne!(&stored[NONCE_LEN..NONCE_LEN + 20], secret.as_slice());
        assert_eq!(decrypt_secret("key one", 7, &stored), Some(secret.clone()));
        assert_eq!(decrypt_secret("key two", 7, &stored), None);
        assert_eq!(decrypt_secret("key one", 8, &stored), None);
        assert_eq!(decrypt_secret("key one", 7, &stored[..5]), None);
        assert_eq!(secret_from_base32(&secret_base32(&secret)), Some(secret));
    }

    #[test]
    fn recovery_codes_are_distinct_and_hash_loosely() {
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        assert!(codes.iter().all(|c| c.len() == 11 && c.as_bytes()[5] == b'-'));
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), RECOVERY_CODE_COUNT);
        assert_eq!(hash_recovery_code("ab2cd-ef3gh"), hash_recovery_code(" AB2CD EF3GH "));
        assert_ne!(hash_recovery_code("ab2cd-ef3gh"), hash_recovery_code("ab2cd-ef3gi"));
    }

    #[test]
    fn required_level_parses() {
        assert_eq!(parse_required_level(""), Some(None));
        assert_eq!(parse_required_level("None"), Some(None));
        assert_eq!(parse_required_level("moderator"), Some(Some(UserLevel::Moderator)));
        assert_eq!(parse_required_level("admin"), Some(Some(UserLevel::SuperAdmin)));
        assert_eq!(parse_required_level("everyone"), None);
        let cfg = MfaConfig { required_level: Some(UserLevel::Moderator), issuer: "Nyaa".into() };
        assert!(cfg.required_for(UserLevel::SuperAdmin) && cfg.required_for(UserLevel::Moderator));
        assert!(!cfg.required_for(UserLevel::Trusted));
        assert!(!MfaConfig::default().required_for(UserLevel::SuperAdmin));
    }

    #[test]
    fn otpauth_url_and_qr() {
        let url = otpauth_url(b"12345678901234567890", "My: Site", "alice");
        assert!(url.starts_with("otpauth://totp/"), "{url}");
        assert!(url.contains("secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"), "{url}");
        assert!(!url.contains("My:"), "{url}");
        assert!(qr_svg(&url).starts_with("<?xml") || qr_svg(&url).contains("<svg"));
    }

    #[test]
    fn codes_work_once() {
        let mut conn = crate::db::connect(":memory:").unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        diesel::sql_query("INSERT INTO users (id, username, password_hash, status) VALUES (1, 'u', 'x', 1)")
            .execute(&mut conn)
            .unwrap();
        let secret = new_secret();
        let codes = new_recovery_codes();
        let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
        UserMfa::enable(&mut conn, 1, &encrypt_secret("k", 1, &secret), 0, &hashes).unwrap();
        let mfa = UserMfa::get(&mut conn, 1).unwrap().unwrap();

        let now = unix_now();
        let code = code_at(&secret, now);
        assert_eq!(mfa.verify(&mut conn, "k", &code).unwrap(), Some(SecondFactor::Totp));
        assert_eq!(mfa.verify(&mut conn, "k", &code).unwrap(), None, "replayed");
        assert_eq!(mfa.verify(&mut conn, "wrong key", &code).unwrap(), None);

        assert_eq!(UserMfa::recovery_codes_left(&mut conn, 1).unwrap(), 10);
        assert_eq!(mfa.verify(&mut conn, "k", &codes[3].to_uppercase()).unwrap(), Some(SecondFactor::RecoveryCode));
        assert_eq!(mfa.verify(&mut conn, "k", &codes[3]).unwrap(), None, "used up");
        assert_eq!(mfa.verify(&mut conn, "k", "nope").unwrap(), None);
        assert_eq!(UserMfa::recovery_codes_left(&mut conn, 1).unwrap(), 9);

        assert!(UserMfa::disable(&mut conn, 1).unwrap());
        assert!(!UserMfa::is_enabled(&mut conn, 1).unwrap());
        assert_eq!(UserMfa::recovery_codes_left(&mut conn, 1).unwrap(), 0);
    }
}
