ALTER TABLE media_derivatives RENAME TO media_derivatives_old;

CREATE TABLE media_derivatives (
    principal              TEXT NOT NULL,
    source_artifact_id     TEXT PRIMARY KEY REFERENCES artifacts(id) ON DELETE CASCADE,
    derivative_artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    created_at             INTEGER NOT NULL
);

INSERT INTO media_derivatives (principal, source_artifact_id, derivative_artifact_id, created_at)
    SELECT principal, source_artifact_id, derivative_artifact_id, created_at
    FROM media_derivatives_old;

DROP TABLE media_derivatives_old;

CREATE INDEX idx_media_derivatives_derivative ON media_derivatives(derivative_artifact_id);
