//! One-shot messages carried across a redirect in the session (Flask's `flash`),
//! shown by `flashes.html`.

use actix_session::Session;
use serde::{Deserialize, Serialize};

const SESSION_KEY: &str = "_flashes";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flash {
    /// Bootstrap alert class suffix: success, danger, info, warning.
    pub category: String,
    /// Bold lead-in, as upstream's `<strong>...</strong>` markup.
    pub strong: String,
    pub text: String,
}

pub fn push(session: &Session, category: &str, strong: &str, text: &str) {
    let mut flashes: Vec<Flash> = session.get(SESSION_KEY).ok().flatten().unwrap_or_default();
    flashes.push(Flash { category: category.into(), strong: strong.into(), text: text.into() });
    session.insert(SESSION_KEY, flashes).ok();
}

/// Returns the pending messages and clears them.
pub fn take(session: &Session) -> Vec<Flash> {
    session.remove_as::<Vec<Flash>>(SESSION_KEY).and_then(Result::ok).unwrap_or_default()
}
