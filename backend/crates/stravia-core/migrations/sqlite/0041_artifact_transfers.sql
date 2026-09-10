ALTER TABLE artifacts ADD COLUMN storage_backend TEXT NOT NULL DEFAULT 'internal';
ALTER TABLE artifacts ADD COLUMN storage_endpoint TEXT;
ALTER TABLE artifacts ADD COLUMN storage_bucket TEXT;
CREATE TABLE artifact_download_grants (
    token_hash TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_artifact_download_grants_hold ON artifact_download_grants(artifact_id, expires_at);
