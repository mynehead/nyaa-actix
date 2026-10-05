use chrono::NaiveDateTime;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

use crate::db::schema::{nyaa_torrents, nyaa_statistics};

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TorrentFlags: i32 {
        const HIDDEN        = 0x01;
        const REMAKE        = 0x04;
        const TRUSTED       = 0x08;
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

    pub fn row_class(&self) -> &'static str {
        if self.is_deleted() || self.is_banned() {
            "danger"
        } else if self.is_remake() {
            "warning"
        } else if self.is_trusted() {
            "success"
        } else {
            ""
        }
    }

    pub fn by_id(conn: &mut SqliteConnection, tid: i32) -> QueryResult<Option<Torrent>> {
        nyaa_torrents::table.find(tid).first(conn).optional()
    }

    pub fn by_info_hash(conn: &mut SqliteConnection, hash: &[u8]) -> QueryResult<Option<Torrent>> {
        nyaa_torrents::table
            .filter(nyaa_torrents::info_hash.eq(hash))
            .first(conn)
            .optional()
    }

    pub fn filesize_human(&self) -> String {
        let size = self.filesize as f64;
        if size >= 1_073_741_824.0 {
            format!("{:.1} GiB", size / 1_073_741_824.0)
        } else if size >= 1_048_576.0 {
            format!("{:.1} MiB", size / 1_048_576.0)
        } else if size >= 1024.0 {
            format!("{:.1} KiB", size / 1024.0)
        } else {
            format!("{} B", self.filesize)
        }
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = nyaa_torrents)]
pub struct NewTorrent {
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
        assert_eq!(torrent(TorrentFlags::empty(), 512).filesize_human(), "512 B");
        assert_eq!(torrent(TorrentFlags::empty(), 1536).filesize_human(), "1.5 KiB");
        assert_eq!(torrent(TorrentFlags::empty(), 5 * 1_048_576).filesize_human(), "5.0 MiB");
        assert_eq!(torrent(TorrentFlags::empty(), 3 * 1_073_741_824).filesize_human(), "3.0 GiB");
    }

    #[test]
    fn row_class_priority() {
        assert_eq!(torrent(TorrentFlags::TRUSTED | TorrentFlags::REMAKE | TorrentFlags::DELETED, 0).row_class(), "danger");
        assert_eq!(torrent(TorrentFlags::TRUSTED | TorrentFlags::REMAKE, 0).row_class(), "warning");
        assert_eq!(torrent(TorrentFlags::TRUSTED, 0).row_class(), "success");
        assert_eq!(torrent(TorrentFlags::empty(), 0).row_class(), "");
    }

    #[test]
    fn magnet_uses_uppercase_hex_hash() {
        let uri = torrent(TorrentFlags::empty(), 0).magnet_uri("x", &[]);
        assert_eq!(uri, format!("magnet:?xt=urn:btih:{}&dn=x", "AB".repeat(20)));
    }
}
