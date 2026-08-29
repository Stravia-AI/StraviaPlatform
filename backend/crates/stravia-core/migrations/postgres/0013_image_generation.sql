ALTER TABLE models
ADD COLUMN operation TEXT NOT NULL DEFAULT 'inference'
CHECK (operation IN ('inference', 'image_generation'));

ALTER TABLE api_keys
ADD COLUMN image_rpm INTEGER;

ALTER TABLE api_keys
ADD COLUMN image_rpd INTEGER;

ALTER TABLE api_keys
ADD COLUMN allow_image_generation BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE artifacts
ADD COLUMN insecure_transport BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE image_continuations (
    id                 TEXT PRIMARY KEY,
    surface            TEXT NOT NULL CHECK (surface IN ('responses', 'mcp')),
    parent_id          TEXT REFERENCES image_continuations(id),
    principal          TEXT NOT NULL,
    route_id           TEXT NOT NULL,
    target_id          TEXT NOT NULL,
    provider_id        TEXT NOT NULL,
    upstream_model     TEXT NOT NULL,
    endpoint           TEXT NOT NULL,
    continuation_mode  TEXT NOT NULL CHECK (continuation_mode IN ('native', 'artifact_replay')),
    opaque_upstream_id TEXT,
    final_artifacts    JSONB NOT NULL,
    created_at         BIGINT NOT NULL,
    expires_at         BIGINT NOT NULL
);

CREATE INDEX idx_image_continuations_principal_expiry
ON image_continuations(principal, expires_at);
CREATE INDEX idx_image_continuations_parent
ON image_continuations(parent_id);

CREATE TABLE artifact_delivery_tokens (
    token_hash  TEXT PRIMARY KEY,
    principal   TEXT NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    expires_at  BIGINT NOT NULL,
    created_at  BIGINT NOT NULL
);

CREATE INDEX idx_artifact_delivery_tokens_expiry
ON artifact_delivery_tokens(expires_at);

CREATE TABLE image_generation_runs (
    id                    TEXT PRIMARY KEY,
    principal             TEXT NOT NULL,
    operation             TEXT NOT NULL DEFAULT 'image_generation',
    route_id              TEXT,
    target_id             TEXT,
    provider_id           TEXT,
    upstream_model        TEXT,
    requested_images      INTEGER NOT NULL,
    generated_images      INTEGER NOT NULL DEFAULT 0,
    failed_images         INTEGER NOT NULL DEFAULT 0,
    outcome               TEXT NOT NULL,
    usage                 JSONB NOT NULL DEFAULT '{}'::jsonb,
    parent_request_id     TEXT,
    capability_drift      JSONB,
    possible_duplicate_charge BOOLEAN NOT NULL DEFAULT FALSE,
    created_at            BIGINT NOT NULL,
    latency_ms            BIGINT,
    completed_at          BIGINT
);

CREATE INDEX idx_image_generation_runs_principal_created
ON image_generation_runs(principal, created_at);

CREATE TABLE image_generation_attempts (
    id                        TEXT PRIMARY KEY,
    run_id                    TEXT NOT NULL REFERENCES image_generation_runs(id) ON DELETE CASCADE,
    ordinal                   INTEGER NOT NULL,
    target_id                 TEXT NOT NULL,
    provider_id               TEXT NOT NULL,
    upstream_model            TEXT NOT NULL,
    outcome                   TEXT NOT NULL,
    error_code                TEXT,
    possible_duplicate_charge BOOLEAN NOT NULL DEFAULT FALSE,
    provider_request_id       TEXT,
    usage                     JSONB,
    latency_ms                BIGINT,
    created_at                BIGINT NOT NULL,
    completed_at              BIGINT,
    UNIQUE(run_id, ordinal)
);

CREATE INDEX idx_image_generation_attempts_run
ON image_generation_attempts(run_id, ordinal);

CREATE TABLE image_capability_drifts (
    id                TEXT PRIMARY KEY,
    provider_id       TEXT NOT NULL,
    upstream_model    TEXT NOT NULL,
    fingerprint       TEXT NOT NULL,
    safe_message      TEXT NOT NULL,
    suppressed_until  BIGINT NOT NULL,
    created_at        BIGINT NOT NULL,
    UNIQUE(provider_id, upstream_model, fingerprint)
);
