//! ALTCHA (altcha.org), a self-hosted proof-of-work captcha, on the login and registration
//! forms. Nothing leaves the site: `/captcha/challenge` hands out a challenge signed with a
//! key derived from SECRET_KEY, the widget (vendored in static/altcha) finds the number that
//! solves it, and the form post carries the solution, which is checked here.
//!
//! This speaks ALTCHA's v1 challenge format, which the v3 widget still solves: SHA-256 of
//! `salt` followed by a secret number in decimal, the salt ending in `?expires=<unix time>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long a challenge can be solved and posted.
const LIFETIME_SECS: i64 = 10 * 60;
const ALGORITHM: &str = "SHA-256";

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct Captcha {
    key: [u8; 32],
    /// CAPTCHA_MAX_NUMBER: the secret number is at most this; higher takes longer to solve.
    pub max_number: u64,
    /// Signatures of solutions already used, with their expiry, so each solves one post.
    used: Arc<Mutex<HashMap<String, i64>>>,
}

impl std::fmt::Debug for Captcha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Captcha(max number {})", self.max_number)
    }
}

/// What `/captcha/challenge` sends the widget.
#[derive(Debug, Serialize, Deserialize)]
pub struct Challenge {
    pub algorithm: String,
    pub challenge: String,
    pub maxnumber: u64,
    pub salt: String,
    pub signature: String,
}

/// What the widget posts back, base64 JSON.
#[derive(Deserialize)]
struct Solution {
    algorithm: String,
    challenge: String,
    number: u64,
    salt: String,
    signature: String,
}

impl Captcha {
    /// None unless USE_CAPTCHA=true.
    pub fn from_env(secret_key: &str) -> Option<Captcha> {
        let var = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let on = var("USE_CAPTCHA")
            .map(|v| v.parse::<bool>().unwrap_or_else(|_| panic!("USE_CAPTCHA: {v:?} is not true or false")))
            .unwrap_or(false);
        let max_number = var("CAPTCHA_MAX_NUMBER").map(|v| {
            v.parse()
                .ok()
                .filter(|&n| n > 0)
                .unwrap_or_else(|| panic!("CAPTCHA_MAX_NUMBER: {v:?} is not a positive number"))
        });
        on.then(|| Captcha::new(secret_key, max_number.unwrap_or(100_000)))
    }

    pub fn new(secret_key: &str, max_number: u64) -> Captcha {
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(None, secret_key.as_bytes())
            .expand(b"nyaa-actix altcha v1", &mut key)
            .expect("32 bytes is a valid HKDF output length");
        Captcha { key, max_number, used: Default::default() }
    }

    fn mac(&self, challenge: &str) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC takes keys of any length");
        mac.update(challenge.as_bytes());
        mac
    }

    /// A fresh challenge, good for [`LIFETIME_SECS`].
    pub fn challenge(&self) -> Challenge {
        let mut salt = [0u8; 12];
        OsRng.fill_bytes(&mut salt);
        let salt = format!("{}?expires={}", hex::encode(salt), chrono::Utc::now().timestamp() + LIFETIME_SECS);
        let number = OsRng.next_u64() % (self.max_number + 1);
        let challenge = hash(&salt, number);
        let signature = hex::encode(self.mac(&challenge).finalize().into_bytes());
        Challenge { algorithm: ALGORITHM.into(), challenge, maxnumber: self.max_number, salt, signature }
    }

    /// Checks the widget's `payload`. Err holds the message for the form.
    pub fn verify(&self, payload: &str) -> Result<(), String> {
        let payload = payload.trim();
        if payload.is_empty() {
            return Err("Please complete the captcha.".into());
        }
        let failed = || "The captcha was not solved. Please try again.".to_string();
        let solution: Solution =
            STANDARD.decode(payload).ok().and_then(|json| serde_json::from_slice(&json).ok()).ok_or_else(failed)?;
        let expires = solution
            .salt
            .split_once('?')
            .and_then(|(_, query)| query.split('&').find_map(|p| p.strip_prefix("expires=")))
            .and_then(|t| t.parse::<i64>().ok())
            .ok_or_else(failed)?;
        let now = chrono::Utc::now().timestamp();
        if expires < now {
            return Err("The captcha expired. Please solve it again.".into());
        }
        let signature = hex::decode(&solution.signature).map_err(|_| failed())?;
        if solution.algorithm != ALGORITHM
            || self.mac(&solution.challenge).verify_slice(&signature).is_err()
            || hash(&solution.salt, solution.number) != solution.challenge
        {
            return Err(failed());
        }
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        used.retain(|_, &mut until| until >= now);
        if used.insert(solution.signature, expires).is_some() {
            return Err(failed());
        }
        Ok(())
    }
}

fn hash(salt: &str, number: u64) -> String {
    hex::encode(Sha256::digest(format!("{salt}{number}")))
}

/// Checks the captcha when it is on. Err holds the message for the form.
pub fn check_form(cfg: &crate::config::Config, payload: &str) -> Result<(), String> {
    cfg.captcha.as_ref().map_or(Ok(()), |c| c.verify(payload))
}

/// GET /captcha/challenge, for the widget. 404 while the captcha is off.
pub async fn challenge(cfg: actix_web::web::Data<crate::config::Config>) -> actix_web::HttpResponse {
    match &cfg.captcha {
        Some(captcha) => {
            actix_web::HttpResponse::Ok().insert_header(("Cache-Control", "no-store")).json(captcha.challenge())
        }
        None => actix_web::HttpResponse::NotFound().finish(),
    }
}

/// Solves `challenge` the way the widget does. For tests.
#[cfg(test)]
pub fn solve(challenge: &Challenge) -> String {
    let number = (0..=challenge.maxnumber).find(|&n| hash(&challenge.salt, n) == challenge.challenge).unwrap();
    let solution = serde_json::json!({
        "algorithm": challenge.algorithm,
        "challenge": challenge.challenge,
        "number": number,
        "salt": challenge.salt,
        "signature": challenge.signature,
        "took": 1,
    });
    STANDARD.encode(solution.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solved_challenges_pass_once() {
        let captcha = Captcha::new("secret", 1000);
        let payload = solve(&captcha.challenge());
        assert_eq!(captcha.verify(&payload), Ok(()));
        assert!(captcha.verify(&payload).is_err(), "a solution works once");
        assert_eq!(captcha.verify(" "), Err("Please complete the captcha.".into()));
        assert!(captcha.verify("not base64!").is_err());
    }

    #[test]
    fn forged_wrong_or_expired_solutions_fail() {
        let captcha = Captcha::new("secret", 1000);
        let tamper = |edit: &dyn Fn(&mut serde_json::Value)| {
            let payload = solve(&captcha.challenge());
            let mut json: serde_json::Value = serde_json::from_slice(&STANDARD.decode(payload).unwrap()).unwrap();
            edit(&mut json);
            captcha.verify(&STANDARD.encode(json.to_string()))
        };
        assert!(tamper(&|j| j["number"] = (j["number"].as_u64().unwrap() + 1).into()).is_err());
        assert!(tamper(&|j| j["algorithm"] = "SHA-1".into()).is_err());
        // Signed with another site's key
        let other = Captcha::new("other secret", 1000);
        assert!(captcha.verify(&solve(&other.challenge())).is_err());
        // Genuinely signed, but past its expiry
        let salt = format!("abc?expires={}", chrono::Utc::now().timestamp() - 1);
        let challenge = hash(&salt, 5);
        let signature = hex::encode(captcha.mac(&challenge).finalize().into_bytes());
        let old = Challenge { algorithm: ALGORITHM.into(), challenge, maxnumber: 10, salt, signature };
        assert_eq!(captcha.verify(&solve(&old)), Err("The captcha expired. Please solve it again.".into()));
    }
}
