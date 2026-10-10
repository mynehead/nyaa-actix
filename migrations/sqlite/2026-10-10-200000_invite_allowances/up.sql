-- Invites a moderator gave a user on their page, on top of INVITES_FOR_TRUSTED. What is
-- left is these plus the level's share minus the codes the user made that are still open
-- or were used (revoked and expired codes come back).
CREATE TABLE invite_allowances (
    user_id INTEGER PRIMARY KEY NOT NULL REFERENCES users(id),
    extra INTEGER NOT NULL DEFAULT 0
);
