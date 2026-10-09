//! Account mails: email verification on registration and password reset links, as upstream.
//! Off unless MAIL_BACKEND is set; `log` writes the mails to the log instead of sending them,
//! for development.

use std::sync::Arc;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

/// Where account mails go.
#[derive(Clone)]
pub enum Mailer {
    Smtp { transport: Arc<SmtpTransport>, from: Mailbox },
    Log { from: Mailbox },
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mailer::Smtp { from, .. } => write!(f, "Mailer::Smtp(from {from})"),
            Mailer::Log { from } => write!(f, "Mailer::Log(from {from})"),
        }
    }
}

/// The mail settings: the mailer and which flows use it.
#[derive(Clone, Debug)]
pub struct MailConfig {
    /// MAIL_BACKEND and friends; None sends no mail.
    pub mailer: Option<Mailer>,
    /// USE_EMAIL_VERIFICATION: new accounts stay inactive until their email link is opened.
    pub use_email_verification: bool,
    /// ALLOW_PASSWORD_RESET: "Forgot your password?" by email (needs a mailer).
    pub allow_password_reset: bool,
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig { mailer: None, use_email_verification: false, allow_password_reset: true }
    }
}

impl MailConfig {
    /// Panics with a readable message on a broken setting, like the rest of the config.
    pub fn from_env() -> Self {
        let var = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let flag = |key: &str, default: bool| var(key).and_then(|v| v.parse().ok()).unwrap_or(default);
        let mailer = Mailer::from_settings(&var).unwrap_or_else(|e| panic!("{e}"));
        let use_email_verification = flag("USE_EMAIL_VERIFICATION", false);
        assert!(
            !use_email_verification || mailer.is_some(),
            "USE_EMAIL_VERIFICATION=true needs MAIL_BACKEND (smtp, or log for development)"
        );
        MailConfig { mailer, use_email_verification, allow_password_reset: flag("ALLOW_PASSWORD_RESET", true) }
    }

    /// The mailer, if new accounts must verify their email.
    pub fn verification(&self) -> Option<&Mailer> {
        self.mailer.as_ref().filter(|_| self.use_email_verification)
    }

    /// The mailer, if users may reset their password by email.
    pub fn password_reset(&self) -> Option<&Mailer> {
        self.mailer.as_ref().filter(|_| self.allow_password_reset)
    }
}

impl Mailer {
    fn from_settings(var: &dyn Fn(&str) -> Option<String>) -> Result<Option<Mailer>, String> {
        let Some(backend) = var("MAIL_BACKEND") else {
            return Ok(None);
        };
        let from: Mailbox = var("MAIL_FROM_ADDRESS")
            .ok_or("MAIL_BACKEND needs MAIL_FROM_ADDRESS")?
            .parse()
            .map_err(|e| format!("MAIL_FROM_ADDRESS: {e}"))?;
        match backend.as_str() {
            "log" => Ok(Some(Mailer::Log { from })),
            "smtp" => {
                let server = var("SMTP_SERVER").ok_or("MAIL_BACKEND=smtp needs SMTP_SERVER")?;
                let security = var("SMTP_SECURITY").unwrap_or_else(|| "starttls".into());
                let builder = match security.as_str() {
                    "starttls" => SmtpTransport::starttls_relay(&server),
                    "tls" => SmtpTransport::relay(&server),
                    "none" => Ok(SmtpTransport::builder_dangerous(&server)),
                    other => return Err(format!("SMTP_SECURITY must be starttls, tls or none, not {other:?}")),
                }
                .map_err(|e| format!("SMTP_SERVER: {e}"))?;
                let default_port = match security.as_str() {
                    "tls" => 465,
                    "none" => 25,
                    _ => 587,
                };
                let port = match var("SMTP_PORT") {
                    Some(p) => p.parse().map_err(|_| format!("SMTP_PORT is not a port: {p:?}"))?,
                    None => default_port,
                };
                let mut builder = builder.port(port);
                if let Some(user) = var("SMTP_USERNAME") {
                    builder = builder.credentials(Credentials::new(user, var("SMTP_PASSWORD").unwrap_or_default()));
                }
                Ok(Some(Mailer::Smtp { transport: Arc::new(builder.build()), from }))
            }
            other => Err(format!("MAIL_BACKEND must be smtp or log, not {other:?}")),
        }
    }

    /// Sends a plain-text mail. Blocking (SMTP), so call it from `web::block`.
    pub fn send(&self, to: &str, subject: &str, body: String) -> Result<(), String> {
        let from = match self {
            Mailer::Smtp { from, .. } | Mailer::Log { from } => from.clone(),
        };
        let to: Mailbox = to.parse().map_err(|e| format!("bad recipient {to:?}: {e}"))?;
        let message = Message::builder()
            .from(from)
            .to(to.clone())
            .subject(subject)
            .body(body.clone())
            .map_err(|e| format!("building mail: {e}"))?;
        match self {
            Mailer::Smtp { transport, .. } => {
                transport.send(&message).map_err(|e| format!("sending mail to {to}: {e}"))?;
            }
            Mailer::Log { .. } => log::info!("Mail to {to}: {subject}\n{body}"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mailer(settings: &[(&str, &str)]) -> Result<Option<Mailer>, String> {
        let map: HashMap<String, String> = settings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Mailer::from_settings(&|key| map.get(key).cloned())
    }

    #[test]
    fn mail_settings() {
        assert!(mailer(&[]).unwrap().is_none(), "off by default");
        assert!(mailer(&[("MAIL_BACKEND", "log")]).unwrap_err().contains("MAIL_FROM_ADDRESS"));
        let from = ("MAIL_FROM_ADDRESS", "Nyaa <noreply@nyaa.test>");
        assert!(matches!(mailer(&[("MAIL_BACKEND", "log"), from]), Ok(Some(Mailer::Log { .. }))));
        assert!(mailer(&[("MAIL_BACKEND", "smtp"), from]).unwrap_err().contains("SMTP_SERVER"));
        let smtp = [("MAIL_BACKEND", "smtp"), from, ("SMTP_SERVER", "mail.nyaa.test")];
        assert!(matches!(mailer(&smtp), Ok(Some(Mailer::Smtp { .. }))));
        let bad = [smtp.as_slice(), &[("SMTP_SECURITY", "ssl")]].concat();
        assert!(mailer(&bad).unwrap_err().contains("SMTP_SECURITY"));
        assert!(mailer(&[("MAIL_BACKEND", "mailgun"), from]).unwrap_err().contains("smtp or log"));
    }

    #[test]
    fn flows_need_a_mailer() {
        let off = MailConfig::default();
        assert!(off.verification().is_none() && off.password_reset().is_none());
        let from: Mailbox = "noreply@nyaa.test".parse().unwrap();
        let on = MailConfig { mailer: Some(Mailer::Log { from }), ..MailConfig::default() };
        assert!(on.verification().is_none(), "verification is opt-in");
        assert!(on.password_reset().is_some());
        assert!(MailConfig { use_email_verification: true, ..on }.verification().is_some());
    }
}
