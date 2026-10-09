use serde::Serialize;

use crate::models::{User, UserLevel};

/// Something a user level allows. Handlers and templates ask for these, never for levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Mark own uploads as trusted (Trusted and up).
    SetTrustedFlag,
    /// Delete, ban and undelete torrents, lock comments, see hidden and deleted torrents.
    ModerateTorrents,
    /// Ban and unban users; only those of a lower level (see [`super::policy::can_ban`]).
    BanUsers,
    /// The admin pages: log, bans, reports, banners, trusted applications.
    ViewAdminPages,
    /// Create torrent groups.
    CreateGroups,
    /// See users' and uploaders' IP addresses.
    SeeIps,
    /// Accept or reject trusted applications.
    DecideTrusted,
    /// Change a lower-ranked user's class between Regular and Trusted, and activate them.
    ChangeUserClass,
    /// Also make users moderators in "Change User Class".
    GrantModerator,
    /// Delete and ban all torrents, or delete all comments, of a lower-ranked user.
    NukeUsers,
    /// Ban and unban whole networks (CIDR) from using the site.
    BanIpRanges,
    /// Read, answer, close and reopen every user's support tickets (/admin/tickets).
    HandleTickets,
    /// Delete other users' comments (upstream: superadmins only).
    DeleteComments,
    /// Upload without the new-account upload rate limit (upstream: trusted users).
    SkipUploadLimit,
    /// Turn off a lower-ranked user's two-factor sign-in, for someone who lost their
    /// authenticator and recovery codes. Not moderators: a taken-over moderator account
    /// must not be able to strip others' second factor.
    ResetTwoFactor,
    /// Make and revoke invite codes on /admin/invites (REGISTRATION_MODE=invite).
    CreateInvites,
}

impl Permission {
    pub const ALL: [Permission; 16] = [
        Permission::SetTrustedFlag,
        Permission::ModerateTorrents,
        Permission::BanUsers,
        Permission::ViewAdminPages,
        Permission::CreateGroups,
        Permission::SeeIps,
        Permission::DecideTrusted,
        Permission::ChangeUserClass,
        Permission::GrantModerator,
        Permission::NukeUsers,
        Permission::BanIpRanges,
        Permission::HandleTickets,
        Permission::DeleteComments,
        Permission::SkipUploadLimit,
        Permission::ResetTwoFactor,
        Permission::CreateInvites,
    ];
}

impl UserLevel {
    /// The one table of what each level may do. Levels are cumulative, as upstream.
    pub fn grants(self, p: Permission) -> bool {
        use Permission::*;
        let needed = match p {
            SetTrustedFlag | SkipUploadLimit => UserLevel::Trusted,
            ModerateTorrents | BanUsers | ViewAdminPages | CreateGroups | ChangeUserClass | HandleTickets
            | CreateInvites => UserLevel::Moderator,
            SeeIps | DecideTrusted | GrantModerator | NukeUsers | BanIpRanges | DeleteComments | ResetTwoFactor => {
                UserLevel::SuperAdmin
            }
        };
        self >= needed
    }
}

impl User {
    pub fn level(&self) -> UserLevel {
        UserLevel::from_i32(self.level)
    }

    pub fn can(&self, p: Permission) -> bool {
        self.level().grants(p)
    }
}

/// `can` in every page's context: `{"moderate_torrents": true, "see_ips": false, ...}`,
/// all false for guests.
pub fn permission_map(user: Option<&User>) -> serde_json::Map<String, serde_json::Value> {
    Permission::ALL
        .iter()
        .map(|&p| {
            let key = serde_json::to_value(p).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
            (key, user.is_some_and(|u| u.can(p)).into())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Permission::*;

    fn granted(level: UserLevel) -> Vec<Permission> {
        Permission::ALL.into_iter().filter(|&p| level.grants(p)).collect()
    }

    #[test]
    fn levels_grant_what_upstream_allows() {
        assert_eq!(granted(UserLevel::Regular), vec![]);
        assert_eq!(granted(UserLevel::Trusted), vec![SetTrustedFlag, SkipUploadLimit]);
        assert_eq!(
            granted(UserLevel::Moderator),
            vec![
                SetTrustedFlag,
                ModerateTorrents,
                BanUsers,
                ViewAdminPages,
                CreateGroups,
                ChangeUserClass,
                HandleTickets,
                SkipUploadLimit,
                CreateInvites
            ]
        );
        assert_eq!(granted(UserLevel::SuperAdmin), Permission::ALL.to_vec());
    }

    #[test]
    fn permission_map_uses_snake_case_keys() {
        let guest = permission_map(None);
        assert_eq!(guest.len(), Permission::ALL.len());
        assert_eq!(guest["view_admin_pages"], false);
        assert_eq!(guest["see_ips"], false);
    }
}
