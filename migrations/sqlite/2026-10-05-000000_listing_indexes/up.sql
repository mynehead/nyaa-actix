-- Listings filtered by category, uploader (profile pages) or group otherwise scan every torrent
CREATE INDEX IF NOT EXISTS nyaa_torrents_category_idx ON nyaa_torrents(main_category_id, sub_category_id);
CREATE INDEX IF NOT EXISTS nyaa_torrents_uploader_idx ON nyaa_torrents(uploader_id);
CREATE INDEX IF NOT EXISTS nyaa_torrents_group_idx ON nyaa_torrents(group_id);
