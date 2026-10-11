-- Torrent pages load comments by torrent, the user comments page and nukes by author,
-- and every POST checks the client IP against the bans.
CREATE INDEX nyaa_comments_torrent_idx ON nyaa_comments (torrent_id, created_time);
CREATE INDEX nyaa_comments_user_idx ON nyaa_comments (user_id);
CREATE INDEX bans_user_ip_idx ON bans (user_ip);
