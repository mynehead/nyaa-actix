-- Torrent reports (upstream nyaa_reports). status: 0 in review, 1 valid (torrent hidden or
-- deleted), 2 invalid (closed without action).
CREATE TABLE nyaa_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    reason TEXT NOT NULL,
    status INTEGER NOT NULL DEFAULT 0,
    torrent_id INTEGER NOT NULL REFERENCES nyaa_torrents(id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id)
);
CREATE INDEX ix_nyaa_reports_status ON nyaa_reports (status, torrent_id);

-- Reports on groups, which upstream does not have; same status values.
CREATE TABLE group_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    reason TEXT NOT NULL,
    status INTEGER NOT NULL DEFAULT 0,
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id)
);
CREATE INDEX ix_group_reports_status ON group_reports (status, group_id);
