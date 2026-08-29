ALTER TABLE models
ADD COLUMN operation TEXT NOT NULL DEFAULT 'inference'
CHECK (operation IN ('inference', 'image_generation'));

ALTER TABLE api_keys
ADD COLUMN image_rpm INTEGER;

ALTER TABLE api_keys
ADD COLUMN image_rpd INTEGER;

ALTER TABLE api_keys
ADD COLUMN allow_image_generation INTEGER NOT NULL DEFAULT 0;

ALTER TABLE artifacts
ADD COLUMN insecure_transport INTEGER NOT NULL DEFAULT 0
CHECK (insecure_transport IN (0, 1));

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
    final_artifacts    TEXT NOT NULL,
    created_at         INTEGER NOT NULL,
    expires_at         INTEGER NOT NULL
);

CREATE INDEX idx_image_continuations_principal_expiry
ON image_continuations(principal, expires_at);
CREATE INDEX idx_image_continuations_parent
ON image_continuations(parent_id);

CREATE TABLE artifact_delivery_tokens (
    token_hash  TEXT PRIMARY KEY,
    principal   TEXT NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    expires_at  INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
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
    usage                 TEXT NOT NULL DEFAULT '{}',
    parent_request_id     TEXT,
    capability_drift      TEXT,
    possible_duplicate_charge INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    latency_ms            INTEGER,
    completed_at          INTEGER
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
    possible_duplicate_charge INTEGER NOT NULL DEFAULT 0,
    provider_request_id       TEXT,
    usage                     TEXT,
    latency_ms                INTEGER,
    created_at                INTEGER NOT NULL,
    completed_at              INTEGER,
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
    suppressed_until  INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    UNIQUE(provider_id, upstream_model, fingerprint)
);
