-- Text shown above the torrent list on the main page while active (Admin > Banners).
CREATE TABLE site_banners (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    content TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT 1,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by INTEGER NOT NULL REFERENCES users(id)
);
