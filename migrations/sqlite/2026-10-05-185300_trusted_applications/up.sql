-- Applications for trusted status (upstream trusted_applications / trusted_reviews).
-- status: 0 new, 1 reviewed, 2 accepted, 3 rejected; 2 and 3 are closed.
CREATE TABLE trusted_applications (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    submitter_id INTEGER NOT NULL REFERENCES users(id),
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    closed_time TIMESTAMP,
    why_want TEXT NOT NULL,
    why_give TEXT NOT NULL,
    status INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX trusted_applications_submitter_idx ON trusted_applications (submitter_id);

-- recommendation: 0 accept, 1 reject, 2 abstain.
CREATE TABLE trusted_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    reviewer_id INTEGER NOT NULL REFERENCES users(id),
    app_id INTEGER NOT NULL REFERENCES trusted_applications(id),
    created_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    comment TEXT NOT NULL,
    recommendation INTEGER NOT NULL
);
CREATE INDEX trusted_reviews_app_idx ON trusted_reviews (app_id);
