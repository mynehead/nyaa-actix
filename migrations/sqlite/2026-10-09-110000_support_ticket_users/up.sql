-- Accounts a ticket is about (User Report), up to 20 per ticket.
CREATE TABLE support_ticket_users (
    ticket_id INTEGER NOT NULL REFERENCES support_tickets(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (ticket_id, user_id)
);
