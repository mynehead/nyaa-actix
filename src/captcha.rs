//! Google reCAPTCHA (v2 checkbox), upstream's USE_RECAPTCHA, on the login and registration
//! forms. Off unless both keys are set. The answer is checked server-side against Google's
//! siteverify endpoint; if Google can't be reached the form is turned back.

use std::time::Duration;

use serde::Deserialize;

const VERIFY_URL: &str = "https://www.google.com/recaptcha/api/siteverify";

#[derive(Clone)]
pub struct Recaptcha {
    /// RECAPTCHA_PUBLIC_KEY: the site key, shown in the page.
    pub public_key: String,
    /// RECAPTCHA_PRIVATE_KEY: the secret, sent only to Google.
    private_key: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Recaptcha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Recaptcha({})", self.public_key)
    }
}

#[derive(Deserialize)]
struct VerifyAnswer {
    success: bool,
    #[serde(default, rename = "error-codes")]
    error_codes: Vec<String>,
}

impl Recaptcha {
    /// None while the keys are unset or USE_RECAPTCHA=false. Panics with a readable message on
    /// USE_RECAPTCHA=true without keys, like the rest of the config.
    pub fn from_env() -> Option<Recaptcha> {
        let var = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let keys = var("RECAPTCHA_PUBLIC_KEY").zip(var("RECAPTCHA_PRIVATE_KEY"));
        let use_recaptcha = var("USE_RECAPTCHA")
            .map(|v| v.parse::<bool>().unwrap_or_else(|_| panic!("USE_RECAPTCHA: {v:?} is not true or false")));
        match (use_recaptcha, keys) {
            (Some(false), _) => None,
            (Some(true), None) => panic!("USE_RECAPTCHA=true needs RECAPTCHA_PUBLIC_KEY and RECAPTCHA_PRIVATE_KEY"),
            (_, keys) => keys.map(|(public_key, private_key)| Recaptcha::new(public_key, private_key)),
        }
    }

    pub fn new(public_key: String, private_key: String) -> Recaptcha {
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
        Recaptcha { public_key, private_key, agent }
    }

    /// Asks Google whether `response` (the widget's answer) is genuine. Blocking: call it
    /// from `web::block`. Err holds the message for the form.
    pub fn verify(&self, response: &str, remote_ip: Option<&str>) -> Result<(), String> {
        if response.trim().is_empty() {
            return Err("Please complete the captcha.".into());
        }
        let mut form = vec![("secret", self.private_key.as_str()), ("response", response)];
        if let Some(ip) = remote_ip {
            form.push(("remoteip", ip));
        }
        let answer: VerifyAnswer =
            self.agent.post(VERIFY_URL).send_form(form).and_then(|mut r| r.body_mut().read_json()).map_err(|e| {
                log::error!("reCAPTCHA siteverify failed: {e}");
                "The captcha could not be checked. Please try again.".to_string()
            })?;
        check(&answer)
    }
}

fn check(answer: &VerifyAnswer) -> Result<(), String> {
    if answer.success {
        return Ok(());
    }
    // Bad keys are the operator's problem, not the visitor's: log them
    if answer.error_codes.iter().any(|c| c.contains("secret")) {
        log::error!("reCAPTCHA rejected RECAPTCHA_PRIVATE_KEY: {:?}", answer.error_codes);
    }
    Err("The captcha was not solved. Please try again.".into())
}

/// Checks the captcha when it is on, off the async runtime. Err holds the message for the form.
pub async fn check_form(
    cfg: &crate::config::Config,
    response: &str,
    remote_ip: Option<String>,
) -> actix_web::Result<Result<(), String>> {
    let Some(recaptcha) = cfg.recaptcha.clone() else {
        return Ok(Ok(()));
    };
    let response = response.to_string();
    Ok(actix_web::web::block(move || recaptcha.verify(&response, remote_ip.as_deref())).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_answers() {
        let answer = |json: &str| check(&serde_json::from_str(json).unwrap());
        assert!(answer(r#"{"success": true, "challenge_ts": "2026-10-09T19:00:00Z", "hostname": "nyaa.si"}"#).is_ok());
        assert!(answer(r#"{"success": false, "error-codes": ["invalid-input-response"]}"#).is_err());
        assert!(answer(r#"{"success": false}"#).is_err());
    }

    #[test]
    fn empty_answer_is_rejected_without_asking_google() {
        let recaptcha = Recaptcha::new("site".into(), "secret".into());
        assert_eq!(recaptcha.verify("  ", None), Err("Please complete the captcha.".into()));
    }
}
