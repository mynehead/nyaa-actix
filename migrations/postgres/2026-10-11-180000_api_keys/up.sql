-- Personal API keys for /api/, stored as SHA-256 hex (they carry 192 random bits each).
CREATE TABLE user_api_keys (
    id SERIAL PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    key_hash TEXT NOT NULL UNIQUE,
    created_time TIMESTAMP NOT NULL,
    last_used_time TIMESTAMP
);
CREATE INDEX ix_user_api_keys_user ON user_api_keys (user_id);
