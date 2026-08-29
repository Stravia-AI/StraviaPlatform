CREATE TABLE artifacts (
    id           TEXT PRIMARY KEY,
    principal    TEXT NOT NULL,
    mime_type    TEXT NOT NULL,
    size         INTEGER NOT NULL CHECK (size >= 0),
    backend_key  TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('staging', 'ready')),
    expires_at   INTEGER NOT NULL,
    created_at   INTEGER NOT NULL
);

CREATE TABLE artifact_uploads (
    id            TEXT PRIMARY KEY,
    artifact_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    principal     TEXT NOT NULL,
    token_hash    TEXT NOT NULL,
    declared_size INTEGER NOT NULL CHECK (declared_size >= 0),
    received_size INTEGER NOT NULL DEFAULT 0 CHECK (received_size >= 0),
    expires_at    INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE artifact_upload_parts (
    upload_id   TEXT NOT NULL REFERENCES artifact_uploads(id) ON DELETE CASCADE,
    part_number INTEGER NOT NULL CHECK (part_number > 0),
    etag        TEXT NOT NULL,
    size        INTEGER NOT NULL CHECK (size >= 0),
    PRIMARY KEY (upload_id, part_number)
);

CREATE INDEX idx_artifacts_expiry ON artifacts(expires_at);
CREATE INDEX idx_artifact_uploads_expiry ON artifact_uploads(expires_at);
