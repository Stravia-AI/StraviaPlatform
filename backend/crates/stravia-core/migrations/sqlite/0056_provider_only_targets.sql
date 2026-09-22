CREATE TABLE model_backends_v56 (
    id                     TEXT PRIMARY KEY,
    model_id               TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id            TEXT NOT NULL REFERENCES providers(id),
    model                  TEXT CHECK (model IS NULL OR length(trim(model)) > 0),
    priority               INTEGER NOT NULL DEFAULT 0,
    created_at             TEXT DEFAULT (datetime('now')),
    thinking_level_map     TEXT NOT NULL DEFAULT '[{"level":"off","control":{"type":"hidden"},"source":"generated"},{"level":"minimal","control":{"type":"hidden"},"source":"generated"},{"level":"low","control":{"type":"hidden"},"source":"generated"},{"level":"medium","control":{"type":"hidden"},"source":"generated"},{"level":"high","control":{"type":"hidden"},"source":"generated"},{"level":"xhigh","control":{"type":"hidden"},"source":"generated"},{"level":"max","control":{"type":"hidden"},"source":"generated"}]'
        CHECK (json_valid(thinking_level_map)),
    first_token_timeout_ms INTEGER NOT NULL DEFAULT 60000,
    target_retry_budget    INTEGER NOT NULL DEFAULT 5,
    target_cooldown_ms     INTEGER NOT NULL DEFAULT 120000,
    enabled                INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1))
);

INSERT INTO model_backends_v56 (
    id,
    model_id,
    provider_id,
    model,
    priority,
    created_at,
    thinking_level_map,
    first_token_timeout_ms,
    target_retry_budget,
    target_cooldown_ms,
    enabled
)
SELECT
    id,
    model_id,
    provider_id,
    model,
    priority,
    created_at,
    thinking_level_map,
    first_token_timeout_ms,
    target_retry_budget,
    target_cooldown_ms,
    enabled
FROM model_backends;

DROP TABLE model_backends;
ALTER TABLE model_backends_v56 RENAME TO model_backends;
CREATE INDEX idx_model_backends_model_id ON model_backends(model_id);
