-- Torrent flag bits now use upstream nyaa's values:
-- ANONYMOUS=1, HIDDEN=2, TRUSTED=4, REMAKE=8 (were HIDDEN=1, ANONYMOUS=2, REMAKE=4, TRUSTED=8).
-- The swap is its own inverse, so down.sql is the same statement.
UPDATE nyaa_torrents SET flags = (flags & ~15)
    | ((flags & 1) << 1) | ((flags & 2) >> 1)
    | ((flags & 4) << 1) | ((flags & 8) >> 1);
