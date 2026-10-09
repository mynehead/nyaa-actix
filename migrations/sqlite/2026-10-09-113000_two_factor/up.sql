-- How a session was signed in: password, password+totp or password+recovery.
ALTER TABLE user_sessions ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'password';

-- Two-factor sign-in (TOTP). The secret is encrypted with a key derived from SECRET_KEY;
-- last_used_step is the newest 30-second step a code was accepted for, so no code works twice.
CREATE TABLE user_mfa (
    user_id INTEGER PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    totp_secret BLOB NOT NULL,
    enabled_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_step BIGINT NOT NULL DEFAULT 0
);

-- One-time recovery codes, stored as SHA-256 hex (they carry 50 random bits each).
CREATE TABLE user_recovery_codes (
    id INTEGER PRIMARY KEY NOT NULL,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    used_time TIMESTAMP
);
CREATE INDEX ix_user_recovery_codes_user ON user_recovery_codes (user_id);
