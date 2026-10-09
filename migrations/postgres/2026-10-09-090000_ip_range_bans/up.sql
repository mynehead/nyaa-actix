-- Networks (CIDR) that may not use the site at all (Admin > Bans). The middleware keeps
-- them in memory; a ban past expires_time no longer applies.
CREATE TABLE ip_range_bans (
    id SERIAL PRIMARY KEY,
    cidr TEXT NOT NULL UNIQUE,
    reason TEXT NOT NULL DEFAULT '',
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_time TIMESTAMP,
    admin_id INTEGER NOT NULL REFERENCES users(id)
);
