CREATE TABLE admin_identity (
    singleton_id SMALLINT PRIMARY KEY CHECK (singleton_id = 1),
    username TEXT UNIQUE,
    password_hash TEXT,
    jwt_secret TEXT NOT NULL,
    credential_revision BIGINT NOT NULL DEFAULT 1 CHECK (credential_revision > 0),
    CHECK ((username IS NULL) = (password_hash IS NULL))
);

CREATE TABLE admin_sessions (
    id TEXT PRIMARY KEY,
    identity_id SMALLINT NOT NULL DEFAULT 1 CHECK (identity_id = 1),
    credential_revision BIGINT NOT NULL,
    refresh_hash TEXT NOT NULL UNIQUE,
    expires_at BIGINT NOT NULL,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY (identity_id) REFERENCES admin_identity(singleton_id) ON DELETE CASCADE
);

CREATE INDEX idx_admin_sessions_expiry ON admin_sessions(expires_at);
