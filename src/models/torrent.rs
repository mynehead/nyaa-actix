use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::auth::Permission;
use crate::db::schema::{nyaa_statistics, nyaa_torrents};
use crate::db::DbConnection;
use crate::models::User;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TorrentFlags: i32 {
        // Upstream nyaa's values, so its database and tooling line up
        const ANONYMOUS     = 0x01;
        const HIDDEN        = 0x02;
        const TRUSTED       = 0x04;
        const REMAKE        = 0x08;
        const COMPLETE      = 0x10;
        const DELETED       = 0x20;
        const BANNED        = 0x40;
        const COMMENT_LOCKED = 0x80;
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = nyaa_torrents)]
pub struct Torrent {
    pub id: i32,
    pub info_hash: Vec<u8>,
    pub display_name: String,
    pub torrent_name: String,
    pub information: String,
    pub description: String,
    pub filesize: i64,
    pub encoding: String,
    pub flags: i32,
    pub uploader_id: Option<i32>,
    /// Never goes into template context; pages that may show it pass it on its own.
    #[serde(skip_serializing, default)]
    pub uploader_ip: Option<Vec<u8>>,
    pub has_torrent: i32,
    pub comment_count: i32,
    pub created_time: NaiveDateTime,
    pub updated_time: NaiveDateTime,
    pub main_category_id: i32,
    pub sub_category_id: i32,
    pub group_id: Option<i32>,
}

impl Torrent {
    pub fn info_hash_hex(&self) -> String {
        hex::encode(&self.info_hash)
    }

    pub fn magnet_uri(&self, display_name: &str, trackers: &[&str]) -> String {
        crate::torrent::magnet::create_magnet(&self.info_hash_hex(), display_name, trackers)
    }

    pub fn is_hidden(&self) -> bool {
        self.flags & TorrentFlags::HIDDEN.bits() != 0
    }

    pub fn is_anonymous(&self) -> bool {
        self.flags & TorrentFlags::ANONYMOUS.bits() != 0
    }

    pub fn is_remake(&self) -> bool {
        self.flags & TorrentFlags::REMAKE.bits() != 0
    }

    pub fn is_trusted(&self) -> bool {
        self.flags & TorrentFlags::TRUSTED.bits() != 0
    }

    pub fn is_complete(&self) -> bool {
        self.flags & TorrentFlags::COMPLETE.bits() != 0
    }

    pub fn is_deleted(&self) -> bool {
        self.flags & TorrentFlags::DELETED.bits() != 0
    }

    pub fn is_banned(&self) -> bool {
        self.flags & TorrentFlags::BANNED.bits() != 0
    }

    /// Listing row class, as upstream: deleted (grey), hidden (orange), remake (red), trusted (green).
    pub fn row_class(&self) -> &'static str {
        if self.is_deleted() || self.is_banned() {
            "deleted"
        } else if self.is_hidden() {
            "warning"
        } else if self.is_remake() {
            "danger"
        } else if self.is_trusted() {
            "success"
        } else {
            "default"
        }
    }

    /// The information field as an IRC or http(s) link when it is one, otherwise
    /// escaped text (upstream `information_as_link`). Returns HTML.
    pub fn information_as_link(&self) -> String {
        let info = self.information.as_str();
        let is_ident = |s: &str, extra: &str| {
            !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c) || extra.contains(c))
        };
        if let Some((chan, server)) = info.strip_prefix('#').and_then(|rest| rest.split_once('@')) {
            if is_ident(chan, "") && is_ident(server, ".:") {
                return format!("<a href=\"irc://{server}/{chan}\">#{chan}@{server}</a>");
            }
        }
        if (info.starts_with("http://") || info.starts_with("https://"))
            && info.len() > info.find("://").unwrap() + 3
            && !info.chars().any(|c| "<>\"".contains(c) || c.is_whitespace())
        {
            let text = percent_encoding::percent_decode_str(info).decode_utf8_lossy();
            return format!("<a rel=\"noopener noreferrer nofollow\" href=\"{}\">{}</a>", escape(info), escape(&text));
        }
        escape(info)
    }

    pub fn by_id(conn: &mut DbConnection, tid: i32) -> QueryResult<Option<Torrent>> {
        nyaa_torrents::table.find(tid).first(conn).optional()
    }

    pub fn by_info_hash(conn: &mut DbConnection, hash: &[u8]) -> QueryResult<Option<Torrent>> {
        nyaa_torrents::table.filter(nyaa_torrents::info_hash.eq(hash)).first(conn).optional()
    }

    pub fn filesize_human(&self) -> String {
        format_filesize(self.filesize)
    }

    pub fn is_comment_locked(&self) -> bool {
        self.flags & TorrentFlags::COMMENT_LOCKED.bits() != 0
    }

    /// Owners and moderators may edit a torrent; once it is deleted only moderators
    /// may (upstream `edit_torrent` and the view page's `can_edit`).
    pub fn can_edit(&self, user: Option<&User>) -> bool {
        match user {
            Some(u) if u.can(Permission::ModerateTorrents) => true,
            Some(u) => Some(u.id) == self.uploader_id && !self.is_deleted() && !self.is_banned(),
            None => false,
        }
    }
}

/// The checkboxes of the edit form.
#[derive(Debug, Default, Clone, Copy)]
pub struct EditFlags {
    pub hidden: bool,
    pub remake: bool,
    pub complete: bool,
    pub anonymous: bool,
    pub trusted: bool,
    pub comment_locked: bool,
}

/// Flags after an edit, as upstream: anyone who may edit sets hidden, remake, complete
/// and anonymous; only trusted users change the trusted flag, and only moderators the
/// comment lock. Deleted and banned are left alone (see [`danger_action`]).
pub fn edited_flags(old: i32, edit: &EditFlags, editor: &User) -> i32 {
    let mut flags = TorrentFlags::from_bits_retain(old);
    flags.set(TorrentFlags::HIDDEN, edit.hidden);
    flags.set(TorrentFlags::REMAKE, edit.remake);
    flags.set(TorrentFlags::COMPLETE, edit.complete);
    flags.set(TorrentFlags::ANONYMOUS, edit.anonymous);
    if editor.can(Permission::SetTrustedFlag) {
        flags.set(TorrentFlags::TRUSTED, edit.trusted);
    }
    if editor.can(Permission::ModerateTorrents) {
        flags.set(TorrentFlags::COMMENT_LOCKED, edit.comment_locked);
    }
    flags.bits()
}

/// A button in the edit page's Danger Zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerAction {
    Delete,
    Ban,
    Undelete,
    Unban,
}

/// New flags and the past-tense action for the flash text (upstream `_delete_torrent`),
/// or `None` when the button doesn't apply to this torrent or editor.
pub fn danger_action(old: i32, action: DangerAction, editor: &User) -> Option<(i32, &'static str)> {
    let mut flags = TorrentFlags::from_bits_retain(old);
    let deleted = flags.contains(TorrentFlags::DELETED);
    let banned = flags.contains(TorrentFlags::BANNED);
    let done = match action {
        DangerAction::Delete if !deleted => {
            flags.insert(TorrentFlags::DELETED);
            "deleted"
        }
        DangerAction::Ban if !banned && editor.can(Permission::ModerateTorrents) => {
            flags.insert(TorrentFlags::DELETED | TorrentFlags::BANNED);
            if deleted {
                "banned"
            } else {
                "deleted and banned"
            }
        }
        DangerAction::Undelete if deleted && editor.can(Permission::ModerateTorrents) => {
            flags.remove(TorrentFlags::DELETED | TorrentFlags::BANNED);
            if banned {
                "undeleted and unbanned"
            } else {
                "undeleted"
            }
        }
        DangerAction::Unban if banned && editor.can(Permission::ModerateTorrents) => {
            flags.remove(TorrentFlags::BANNED);
            "unbanned"
        }
        _ => return None,
    };
    Some((flags.bits(), done))
}

/// Binary-unit size, like Jinja's `filesizeformat(True)` that upstream uses.
pub fn format_filesize(bytes: i64) -> String {
    let size = bytes as f64;
    match bytes {
        1 => "1 Byte".to_string(),
        b if b < 1024 => format!("{} Bytes", b),
        _ => {
            let units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
            let mut unit = 1024.0_f64;
            for (i, name) in units.iter().enumerate() {
                if size < unit * 1024.0 || i == units.len() - 1 {
                    return format!("{:.1} {}", size / unit, name);
                }
                unit *= 1024.0;
            }
            unreachable!()
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

#[derive(Debug, Insertable)]
#[diesel(table_name = nyaa_torrents)]
pub struct NewTorrent {
    /// Set when a reupload replaces a deleted torrent, so it keeps its id (as upstream).
    pub id: Option<i32>,
    pub info_hash: Vec<u8>,
    pub display_name: String,
    pub torrent_name: String,
    pub information: String,
    pub description: String,
    pub filesize: i64,
    pub encoding: String,
    pub flags: i32,
    pub uploader_id: Option<i32>,
    pub uploader_ip: Option<Vec<u8>>,
    pub has_torrent: i32,
    pub comment_count: i32,
    pub created_time: NaiveDateTime,
    pub updated_time: NaiveDateTime,
    pub main_category_id: i32,
    pub sub_category_id: i32,
    pub group_id: Option<i32>,
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = nyaa_statistics)]
pub struct Statistic {
    pub torrent_id: i32,
    pub seed_count: i32,
    pub leech_count: i32,
    pub download_count: i32,
    pub last_updated: NaiveDateTime,
    /// The tracker's completed count at the last stats sync (see `crate::tracker`).
    #[serde(skip)]
    pub tracker_completed: i32,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = nyaa_statistics)]
pub struct NewStatistic {
    pub torrent_id: i32,
    pub seed_count: i32,
    pub leech_count: i32,
    pub download_count: i32,
    pub last_updated: NaiveDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(flags: TorrentFlags, filesize: i64) -> Torrent {
        Torrent { flags: flags.bits(), filesize, ..crate::torrent::tests::sample_torrent() }
    }

    #[test]
    fn human_file_sizes() {
        assert_eq!(torrent(TorrentFlags::empty(), 512).filesize_human(), "512 Bytes");
        assert_eq!(torrent(TorrentFlags::empty(), 1536).filesize_human(), "1.5 KiB");
        assert_eq!(torrent(TorrentFlags::empty(), 5 * 1_048_576).filesize_human(), "5.0 MiB");
        assert_eq!(torrent(TorrentFlags::empty(), 3 * 1_073_741_824).filesize_human(), "3.0 GiB");
        assert_eq!(format_filesize(5 << 40), "5.0 TiB");
    }

    #[test]
    fn information_links() {
        let info =
            |s: &str| Torrent { information: s.into(), ..torrent(TorrentFlags::empty(), 0) }.information_as_link();
        assert_eq!(info("#chan@irc.rizon.net"), "<a href=\"irc://irc.rizon.net/chan\">#chan@irc.rizon.net</a>");
        assert_eq!(
            info("https://a.b/x%20y"),
            "<a rel=\"noopener noreferrer nofollow\" href=\"https://a.b/x%20y\">https://a.b/x y</a>"
        );
        assert_eq!(info("https://a.b/\"><script>"), "https://a.b/&quot;&gt;&lt;script&gt;");
        assert_eq!(info("<b>hi</b>"), "&lt;b&gt;hi&lt;/b&gt;");
    }

    #[test]
    fn row_class_priority() {
        assert_eq!(
            torrent(TorrentFlags::TRUSTED | TorrentFlags::REMAKE | TorrentFlags::DELETED, 0).row_class(),
            "deleted"
        );
        assert_eq!(torrent(TorrentFlags::HIDDEN | TorrentFlags::REMAKE, 0).row_class(), "warning");
        assert_eq!(torrent(TorrentFlags::TRUSTED | TorrentFlags::REMAKE, 0).row_class(), "danger");
        assert_eq!(torrent(TorrentFlags::TRUSTED, 0).row_class(), "success");
        assert_eq!(torrent(TorrentFlags::empty(), 0).row_class(), "default");
    }

    fn user(id: i32, level: i32) -> User {
        User {
            id,
            username: format!("u{id}"),
            email: None,
            password_hash: String::new(),
            status: 1,
            level,
            created_time: NaiveDateTime::default(),
            last_login_date: None,
            last_login_ip: None,
            registration_ip: None,
            avatar_time: None,
        }
    }

    #[test]
    fn owners_and_moderators_can_edit() {
        let mut t = Torrent { uploader_id: Some(1), ..torrent(TorrentFlags::empty(), 0) };
        assert!(t.can_edit(Some(&user(1, 0))));
        assert!(!t.can_edit(Some(&user(2, 1))), "trusted isn't enough for someone else's torrent");
        assert!(t.can_edit(Some(&user(2, 2))));
        assert!(!t.can_edit(None));
        t.flags = TorrentFlags::DELETED.bits();
        assert!(!t.can_edit(Some(&user(1, 0))), "deleted torrents are moderator-only");
        assert!(t.can_edit(Some(&user(2, 2))));
    }

    #[test]
    fn edit_keeps_flags_the_editor_may_not_change() {
        let old = (TorrentFlags::TRUSTED | TorrentFlags::COMMENT_LOCKED | TorrentFlags::HIDDEN).bits();
        let edit = EditFlags { remake: true, ..Default::default() };
        // A regular owner can't drop trusted or the comment lock
        assert_eq!(
            edited_flags(old, &edit, &user(1, 0)),
            (TorrentFlags::TRUSTED | TorrentFlags::COMMENT_LOCKED | TorrentFlags::REMAKE).bits()
        );
        // Trusted users set trusted; moderators also set the lock
        assert_eq!(edited_flags(old, &edit, &user(1, 1)), (TorrentFlags::COMMENT_LOCKED | TorrentFlags::REMAKE).bits());
        assert_eq!(edited_flags(old, &edit, &user(1, 2)), TorrentFlags::REMAKE.bits());
        // Deleted and banned are never touched by an edit
        let deleted = (TorrentFlags::DELETED | TorrentFlags::BANNED).bits();
        assert_eq!(edited_flags(deleted, &EditFlags::default(), &user(1, 3)), deleted);
    }

    #[test]
    fn danger_zone_actions() {
        let (owner, moderator) = (user(1, 0), user(2, 2));
        let none = TorrentFlags::empty().bits();
        let deleted = TorrentFlags::DELETED.bits();
        let banned = (TorrentFlags::DELETED | TorrentFlags::BANNED).bits();
        assert_eq!(danger_action(none, DangerAction::Delete, &owner), Some((deleted, "deleted")));
        assert_eq!(danger_action(deleted, DangerAction::Delete, &owner), None);
        assert_eq!(danger_action(none, DangerAction::Ban, &owner), None, "only moderators ban");
        assert_eq!(danger_action(none, DangerAction::Ban, &moderator), Some((banned, "deleted and banned")));
        assert_eq!(danger_action(deleted, DangerAction::Ban, &moderator), Some((banned, "banned")));
        assert_eq!(danger_action(banned, DangerAction::Undelete, &moderator), Some((none, "undeleted and unbanned")));
        assert_eq!(danger_action(deleted, DangerAction::Undelete, &owner), None);
        assert_eq!(danger_action(banned, DangerAction::Unban, &moderator), Some((deleted, "unbanned")));
        assert_eq!(danger_action(deleted, DangerAction::Unban, &moderator), None);
    }

    #[test]
    fn magnet_uses_uppercase_hex_hash() {
        let uri = torrent(TorrentFlags::empty(), 0).magnet_uri("x", &[]);
        assert_eq!(uri, format!("magnet:?xt=urn:btih:{}&dn=x", "AB".repeat(20)));
    }
}
