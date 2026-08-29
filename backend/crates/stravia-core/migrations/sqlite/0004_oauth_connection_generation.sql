DROP INDEX idx_oauth_creds_status;
DROP INDEX idx_oauth_creds_expires;

ALTER TABLE provider_oauth_credentials RENAME TO provider_oauth_credentials_old;

CREATE TABLE provider_oauth_credentials (
    provider_id     TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    connection_id   TEXT NOT NULL,
    driver_key      TEXT NOT NULL DEFAULT '',
    scheme          TEXT NOT NULL DEFAULT '',
    access_token    TEXT NOT NULL DEFAULT '',
    refresh_token   TEXT,
    expires_at      TEXT,
    resource_url    TEXT,
    subject_id      TEXT,
    scopes          TEXT NOT NULL DEFAULT '[]',
    meta            TEXT NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL DEFAULT 'connected',
    status_version  INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    last_refresh_at TEXT,
    created_at      TEXT DEFAULT (datetime('now')),
    updated_at      TEXT DEFAULT (datetime('now'))
);

INSERT INTO provider_oauth_credentials (
    provider_id, connection_id, driver_key, scheme, access_token, refresh_token,
    expires_at, resource_url, subject_id, scopes, meta, status, status_version,
    last_error, last_refresh_at, created_at, updated_at
)
SELECT
    provider_id, printf('legacy-%032x', rowid), driver_key, scheme, access_token, refresh_token,
    expires_at, resource_url, subject_id, scopes, meta, status, status_version,
    last_error, last_refresh_at, created_at, updated_at
FROM provider_oauth_credentials_old;

DROP TABLE provider_oauth_credentials_old;

CREATE UNIQUE INDEX idx_oauth_creds_connection_id ON provider_oauth_credentials(connection_id);
CREATE INDEX idx_oauth_creds_status ON provider_oauth_credentials(status);
CREATE INDEX idx_oauth_creds_expires ON provider_oauth_credentials(expires_at);
