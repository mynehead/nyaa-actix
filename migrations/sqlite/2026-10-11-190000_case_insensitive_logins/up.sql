-- Logins, password resets and the "email in use" check compare lower(...) (models/user.rs)
CREATE INDEX users_lower_username_idx ON users (lower(username));
CREATE INDEX users_lower_email_idx ON users (lower(email));
