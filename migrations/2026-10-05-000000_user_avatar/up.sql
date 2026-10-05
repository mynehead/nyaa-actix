-- When the user last uploaded an avatar; NULL means none (Gravatar or the default is shown).
-- Also busts browser caches of /avatar/{id}.
ALTER TABLE users ADD COLUMN avatar_time TIMESTAMP;
