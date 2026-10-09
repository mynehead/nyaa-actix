//! Rules that depend on the object as well as the user's level.

use super::Permission;
use crate::models::{Comment, Torrent, User};

/// Upstream shows the user page's Danger Zone to moderators above the user's level.
pub fn can_ban(moderator: &User, user: &User) -> bool {
    moderator.can(Permission::BanUsers) && moderator.level() > user.level()
}

/// Upstream shows the comment form to logged-in users, and on locked torrents only to moderators.
pub fn can_comment(torrent: &Torrent, user: Option<&User>) -> bool {
    user.is_some_and(|u| !torrent.is_comment_locked() || u.can(Permission::ModerateTorrents))
}

/// Upstream lets a comment's author edit it until `EDITING_TIME_LIMIT` (`limit` seconds) runs
/// out, and on a locked torrent only if they may still post there. Moderators and admins
/// have no time limit on their own comments.
pub fn can_edit_comment(comment: &Comment, torrent: &Torrent, user: Option<&User>, limit: i64) -> bool {
    user.is_some_and(|u| {
        comment.user_id == Some(u.id)
            && can_comment(torrent, user)
            && !comment.editing_limit_exceeded(edit_time_limit(u, limit))
    })
}

/// The comment editing time limit that applies to `user`: none (0) for moderators and up.
pub fn edit_time_limit(user: &User, limit: i64) -> i64 {
    if user.can(Permission::ModerateTorrents) {
        0
    } else {
        limit
    }
}

/// The author may delete while they may edit; superadmins delete any comment.
pub fn can_delete_comment(comment: &Comment, torrent: &Torrent, user: Option<&User>, limit: i64) -> bool {
    user.is_some_and(|u| u.can(Permission::DeleteComments)) || can_edit_comment(comment, torrent, user, limit)
}
