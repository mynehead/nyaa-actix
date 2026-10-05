-- PostgreSQL version of migrations/sqlite/2024-01-01-000000_initial. Column types map to the
-- same Diesel types (SERIAL = Integer, BYTEA = Binary) so one src/db/schema.rs fits both.
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    email TEXT UNIQUE,
    password_hash TEXT NOT NULL,
    status INTEGER NOT NULL DEFAULT 0,
    level INTEGER NOT NULL DEFAULT 0,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_login_date TIMESTAMP,
    last_login_ip BYTEA,
    registration_ip BYTEA
);

CREATE TABLE groups (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    tag TEXT NOT NULL UNIQUE,
    slug TEXT NOT NULL UNIQUE,
    description TEXT,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    owner_id INTEGER NOT NULL REFERENCES users(id)
);

CREATE TABLE group_members (
    group_id INTEGER NOT NULL REFERENCES groups(id),
    user_id INTEGER NOT NULL REFERENCES users(id),
    permissions INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (group_id, user_id)
);

CREATE TABLE nyaa_main_categories (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE nyaa_sub_categories (
    id INTEGER NOT NULL,
    main_category_id INTEGER NOT NULL REFERENCES nyaa_main_categories(id),
    name TEXT NOT NULL,
    PRIMARY KEY (id, main_category_id)
);

CREATE TABLE nyaa_torrents (
    id SERIAL PRIMARY KEY,
    info_hash BYTEA NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    torrent_name TEXT NOT NULL,
    information TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    filesize BIGINT NOT NULL DEFAULT 0,
    encoding TEXT NOT NULL DEFAULT 'utf-8',
    flags INTEGER NOT NULL DEFAULT 0,
    uploader_id INTEGER REFERENCES users(id),
    uploader_ip BYTEA,
    has_torrent INTEGER NOT NULL DEFAULT 0,
    comment_count INTEGER NOT NULL DEFAULT 0,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    main_category_id INTEGER NOT NULL,
    sub_category_id INTEGER NOT NULL,
    group_id INTEGER REFERENCES groups(id),
    FOREIGN KEY (main_category_id, sub_category_id)
        REFERENCES nyaa_sub_categories(main_category_id, id)
);

CREATE INDEX nyaa_torrents_display_name_idx ON nyaa_torrents(display_name);
CREATE INDEX nyaa_torrents_flags_idx ON nyaa_torrents(flags);
CREATE INDEX nyaa_torrents_filesize_idx ON nyaa_torrents(filesize);

CREATE TABLE nyaa_statistics (
    torrent_id INTEGER PRIMARY KEY NOT NULL REFERENCES nyaa_torrents(id),
    seed_count INTEGER NOT NULL DEFAULT 0,
    leech_count INTEGER NOT NULL DEFAULT 0,
    download_count INTEGER NOT NULL DEFAULT 0,
    last_updated TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE nyaa_comments (
    id SERIAL PRIMARY KEY,
    torrent_id INTEGER NOT NULL REFERENCES nyaa_torrents(id),
    user_id INTEGER REFERENCES users(id),
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    edited_time TIMESTAMP,
    text TEXT NOT NULL
);

CREATE TABLE bans (
    id SERIAL PRIMARY KEY,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    admin_id INTEGER NOT NULL REFERENCES users(id),
    user_id INTEGER REFERENCES users(id),
    user_ip BYTEA,
    reason TEXT NOT NULL DEFAULT ''
);

CREATE TABLE user_preferences (
    user_id INTEGER PRIMARY KEY NOT NULL REFERENCES users(id),
    hide_comments INTEGER NOT NULL DEFAULT 0
);

-- Seed default categories
INSERT INTO nyaa_main_categories (id, name) VALUES
    (1, 'Anime'),
    (2, 'Audio'),
    (3, 'Literature'),
    (4, 'Live Action'),
    (5, 'Pictures'),
    (6, 'Software');

INSERT INTO nyaa_sub_categories (id, main_category_id, name) VALUES
    (0, 1, 'Anime - All'), (1, 1, 'Anime Music Video'), (2, 1, 'English-translated'),
    (3, 1, 'Non-English-translated'), (4, 1, 'Raw'),
    (0, 2, 'Audio - All'), (1, 2, 'Lossless'), (2, 2, 'Lossy'),
    (0, 3, 'Literature - All'), (1, 3, 'English-translated'), (2, 3, 'Non-English-translated'), (3, 3, 'Raw'),
    (0, 4, 'Live Action - All'), (1, 4, 'English-translated'), (2, 4, 'Idol/Promotional Video'), (3, 4, 'Non-English-translated'), (4, 4, 'Raw'),
    (0, 5, 'Pictures - All'), (1, 5, 'Graphics'), (2, 5, 'Photos'),
    (0, 6, 'Software - All'), (1, 6, 'Applications'), (2, 6, 'Games');

-- The rows above set ids explicitly, so move the sequence past them
SELECT setval('nyaa_main_categories_id_seq', (SELECT MAX(id) FROM nyaa_main_categories));
