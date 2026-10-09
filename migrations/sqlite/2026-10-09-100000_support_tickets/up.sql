-- Support tickets: a user writes to the staff, both sides reply until it is closed.
-- category: torrent, group, user, comment or other (as on AniRena). status: 0 open, 1 closed. staff_replied: whether the last message is from staff, so the
-- queue can show which tickets wait on staff and the user sees which ones were answered.
CREATE TABLE support_tickets (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    category TEXT NOT NULL DEFAULT 'other',
    subject TEXT NOT NULL,
    status INTEGER NOT NULL DEFAULT 0,
    staff_replied BOOLEAN NOT NULL DEFAULT FALSE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX ix_support_tickets_status ON support_tickets (status, category, updated_time);
CREATE INDEX ix_support_tickets_user ON support_tickets (user_id, created_time);

-- The ticket's messages, the first one written with the ticket. from_staff is kept per
-- message so a later level change doesn't relabel old replies.
CREATE TABLE support_ticket_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    body TEXT NOT NULL,
    from_staff BOOLEAN NOT NULL DEFAULT FALSE,
    ticket_id INTEGER NOT NULL REFERENCES support_tickets(id) ON DELETE CASCADE,
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL
);
CREATE INDEX ix_support_ticket_messages_ticket ON support_ticket_messages (ticket_id, id);
CREATE INDEX ix_support_ticket_messages_user ON support_ticket_messages (user_id, created_time);

-- Torrents a ticket is about (Torrent Report), up to 50 per ticket.
CREATE TABLE support_ticket_torrents (
    ticket_id INTEGER NOT NULL REFERENCES support_tickets(id) ON DELETE CASCADE,
    torrent_id INTEGER NOT NULL REFERENCES nyaa_torrents(id) ON DELETE CASCADE,
    PRIMARY KEY (ticket_id, torrent_id)
);
