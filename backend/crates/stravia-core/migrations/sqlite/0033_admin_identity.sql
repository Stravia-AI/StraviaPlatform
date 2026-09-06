CREATE TABLE admin_identity (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    username TEXT UNIQUE,
    password_hash TEXT,
    jwt_secret TEXT NOT NULL,
    credential_revision INTEGER NOT NULL DEFAULT 1 CHECK (credential_revision > 0),
    CHECK ((username IS NULL) = (password_hash IS NULL))
);

CREATE TABLE admin_sessions (
    id TEXT PRIMARY KEY,
    identity_id INTEGER NOT NULL DEFAULT 1 CHECK (identity_id = 1),
    credential_revision INTEGER NOT NULL,
    refresh_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0 CHECK (revoked IN (0, 1)),
    FOREIGN KEY (identity_id) REFERENCES admin_identity(singleton_id) ON DELETE CASCADE
);

CREATE INDEX idx_admin_sessions_expiry ON admin_sessions(expires_at);
