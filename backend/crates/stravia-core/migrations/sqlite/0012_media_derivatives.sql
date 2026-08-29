CREATE TABLE media_derivatives (
    principal              TEXT NOT NULL,
    source_artifact_id     TEXT PRIMARY KEY REFERENCES artifacts(id) ON DELETE CASCADE,
    derivative_artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(id) ON DELETE CASCADE,
    created_at             INTEGER NOT NULL,
    CHECK (source_artifact_id <> derivative_artifact_id)
);
