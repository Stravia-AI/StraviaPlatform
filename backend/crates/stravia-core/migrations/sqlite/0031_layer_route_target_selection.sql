PRAGMA legacy_alter_table = ON;

ALTER TABLE models RENAME TO models_v31;

CREATE TABLE models (
    id           TEXT PRIMARY KEY,
    model_id     TEXT NOT NULL,
    balance      TEXT DEFAULT 'traffic_equalization',
    is_enabled   INTEGER DEFAULT 1,
    priority     INTEGER DEFAULT 0,
    created_at   TEXT DEFAULT (datetime('now')),
    display_name TEXT
);

INSERT INTO models (
    id,
    model_id,
    balance,
    is_enabled,
    priority,
    created_at,
    display_name
)
SELECT
    id,
    model_id,
    CASE
        WHEN lower(trim(COALESCE(balance, ''))) = 'latency' THEN 'latency_preference'
        ELSE 'traffic_equalization'
    END,
    is_enabled,
    priority,
    created_at,
    display_name
FROM models_v31;

CREATE TABLE model_backends_v31 (
    id                     TEXT PRIMARY KEY,
    model_id               TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id            TEXT NOT NULL REFERENCES providers(id),
    model                  TEXT NOT NULL,
    priority               INTEGER NOT NULL DEFAULT 0,
    created_at             TEXT DEFAULT (datetime('now')),
    thinking_level_map     TEXT NOT NULL DEFAULT '[{"level":"off","control":{"type":"hidden"},"source":"generated"},{"level":"minimal","control":{"type":"hidden"},"source":"generated"},{"level":"low","control":{"type":"hidden"},"source":"generated"},{"level":"medium","control":{"type":"hidden"},"source":"generated"},{"level":"high","control":{"type":"hidden"},"source":"generated"},{"level":"xhigh","control":{"type":"hidden"},"source":"generated"},{"level":"max","control":{"type":"hidden"},"source":"generated"}]'
        CHECK (json_valid(thinking_level_map)),
    first_token_timeout_ms INTEGER NOT NULL DEFAULT 60000,
    target_retry_budget    INTEGER NOT NULL DEFAULT 5,
    target_cooldown_ms     INTEGER NOT NULL DEFAULT 120000
);

INSERT INTO model_backends_v31 (
    id,
    model_id,
    provider_id,
    model,
    priority,
    created_at,
    thinking_level_map
)
SELECT
    id,
    model_id,
    provider_id,
    model,
    0,
    created_at,
    thinking_level_map
FROM model_backends;

DROP TABLE model_backends;
ALTER TABLE model_backends_v31 RENAME TO model_backends;
CREATE INDEX idx_model_backends_model_id ON model_backends(model_id);

CREATE TABLE api_key_models_v31 (
    api_key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model_id   TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    PRIMARY KEY (api_key_id, model_id)
);

INSERT INTO api_key_models_v31 (api_key_id, model_id)
SELECT api_key_id, model_id FROM api_key_models;

DROP TABLE api_key_models;
ALTER TABLE api_key_models_v31 RENAME TO api_key_models;
CREATE INDEX idx_api_key_models_model_id ON api_key_models(model_id);

CREATE TABLE agent_definition_configs_v31 (
    definition_id TEXT PRIMARY KEY,
    enabled       INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    model_id      TEXT REFERENCES models(id) ON DELETE SET NULL,
    updated_at    INTEGER NOT NULL,
    thinking_level TEXT
        CHECK (thinking_level IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'))
);

INSERT INTO agent_definition_configs_v31 (
    definition_id,
    enabled,
    model_id,
    updated_at,
    thinking_level
)
SELECT definition_id, enabled, model_id, updated_at, thinking_level
FROM agent_definition_configs;

DROP TABLE agent_definition_configs;
ALTER TABLE agent_definition_configs_v31 RENAME TO agent_definition_configs;

DROP TABLE models_v31;
CREATE UNIQUE INDEX idx_models_route_id ON models(model_id);

PRAGMA legacy_alter_table = OFF;
