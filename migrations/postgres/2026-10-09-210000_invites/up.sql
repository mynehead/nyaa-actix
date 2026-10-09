-- Invite codes for REGISTRATION_MODE=invite (Admin > Invites). Only a SHA-256 of the code
-- is stored; the code itself is shown once, when it is made. used_by also records who
-- invited whom (inviter_id -> used_by).
CREATE TABLE invites (
    id SERIAL PRIMARY KEY,
    code_hash TEXT NOT NULL UNIQUE,
    inviter_id INTEGER NOT NULL REFERENCES users(id),
    email TEXT,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_time TIMESTAMP NOT NULL,
    used_by INTEGER UNIQUE REFERENCES users(id),
    used_time TIMESTAMP,
    revoked BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE INDEX invites_inviter_id ON invites (inviter_id);
