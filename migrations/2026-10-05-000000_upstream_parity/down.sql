DROP TABLE user_sessions;

DROP INDEX nyaa_torrents_comment_count_idx;
DROP INDEX nyaa_statistics_download_count_idx;
DROP INDEX nyaa_statistics_leech_count_idx;
DROP INDEX nyaa_statistics_seed_count_idx;

-- Torrents moved off the "All" rows stay in their new sub category.
INSERT INTO nyaa_sub_categories (id, main_category_id, name) VALUES
    (0, 1, 'Anime - All'), (0, 2, 'Audio - All'), (0, 3, 'Literature - All'),
    (0, 4, 'Live Action - All'), (0, 5, 'Pictures - All'), (0, 6, 'Software - All');

UPDATE nyaa_torrents SET flags = (flags & ~15)
    | ((flags & 1) << 1) | ((flags & 2) >> 1)
    | ((flags & 4) << 1) | ((flags & 8) >> 1);
