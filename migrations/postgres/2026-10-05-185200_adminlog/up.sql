-- Moderator actions shown on /admin/log, as upstream's nyaa_adminlog. `log` is Markdown.
CREATE TABLE adminlog (
    id SERIAL PRIMARY KEY,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    log VARCHAR(1024) NOT NULL,
    admin_id INTEGER NOT NULL REFERENCES users(id)
);
CREATE INDEX ix_adminlog_created_time ON adminlog (created_time);
