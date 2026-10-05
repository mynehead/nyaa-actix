-- Torrent flag bits now use upstream nyaa's values:
-- ANONYMOUS=1, HIDDEN=2, TRUSTED=4, REMAKE=8 (were HIDDEN=1, ANONYMOUS=2, REMAKE=4, TRUSTED=8).
UPDATE nyaa_torrents SET flags = (flags & ~15)
    | ((flags & 1) << 1) | ((flags & 2) >> 1)
    | ((flags & 4) << 1) | ((flags & 8) >> 1);

-- Like upstream, "Main - All" is only a search bucket (c=1_0), never a stored sub category.
-- Torrents filed under one move to that main category's first real sub category.
UPDATE nyaa_torrents SET sub_category_id = (
    SELECT MIN(s.id) FROM nyaa_sub_categories s
    WHERE s.main_category_id = nyaa_torrents.main_category_id AND s.id > 0
) WHERE sub_category_id = 0;
DELETE FROM nyaa_sub_categories WHERE id = 0;

-- Sorting by stats and comment count, as upstream indexes them.
CREATE INDEX nyaa_statistics_seed_count_idx ON nyaa_statistics(seed_count);
CREATE INDEX nyaa_statistics_leech_count_idx ON nyaa_statistics(leech_count);
CREATE INDEX nyaa_statistics_download_count_idx ON nyaa_statistics(download_count);
CREATE INDEX nyaa_torrents_comment_count_idx ON nyaa_torrents(comment_count);

-- Server-side sessions: the cookie only carries the id, so logout and revocation take effect.
CREATE TABLE user_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id INTEGER NOT NULL REFERENCES users(id),
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_seen TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    ip BLOB
);
CREATE INDEX user_sessions_user_id_idx ON user_sessions(user_id);
