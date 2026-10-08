//! Rules that depend on the object as well as the user's level.

use super::Permission;
use crate::models::{Torrent, User};

/// Upstream shows the user page's Danger Zone to moderators above the user's level.
pub fn can_ban(moderator: &User, user: &User) -> bool {
    moderator.can(Permission::BanUsers) && moderator.level() > user.level()
}

/// Upstream shows the comment form to logged-in users, and on locked torrents only to moderators.
pub fn can_comment(torrent: &Torrent, user: Option<&User>) -> bool {
    user.is_some_and(|u| !torrent.is_comment_locked() || u.can(Permission::ModerateTorrents))
}
