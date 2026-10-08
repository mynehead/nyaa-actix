-- The tracker's own completed count at the last stats sync, so each sync adds only the
-- downloads since then to download_count (and a tracker restart, which starts counting
-- from zero again, doesn't lower it).
ALTER TABLE nyaa_statistics ADD COLUMN tracker_completed INTEGER NOT NULL DEFAULT 0;
