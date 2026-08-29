CREATE TABLE provider_models (
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('discovered', 'manual')),
    metadata_source_provider_id TEXT,
    presence TEXT NOT NULL CHECK (presence IN ('present', 'missing')),
    lifecycle_status TEXT CHECK (lifecycle_status IN ('alpha', 'beta', 'deprecated')),
    selection_policy TEXT NOT NULL DEFAULT 'auto'
        CHECK (selection_policy IN ('auto', 'force_enabled', 'force_disabled')),
    name TEXT,
    family TEXT,
    attachment BOOLEAN,
    reasoning BOOLEAN,
    tool_call BOOLEAN,
    open_weights BOOLEAN,
    structured_output BOOLEAN,
    temperature BOOLEAN,
    limit_context BIGINT CHECK (limit_context IS NULL OR limit_context >= 0),
    limit_input BIGINT CHECK (limit_input IS NULL OR limit_input >= 0),
    limit_output BIGINT CHECK (limit_output IS NULL OR limit_output >= 0),
    cost_input NUMERIC,
    cost_output NUMERIC,
    cost_reasoning NUMERIC,
    cost_cache_read NUMERIC,
    cost_cache_write NUMERIC,
    cost_input_audio NUMERIC,
    cost_output_audio NUMERIC,
    metadata_json JSONB NOT NULL
        CHECK (jsonb_typeof(metadata_json) = 'object'),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider_id, model_id)
);

CREATE INDEX idx_provider_models_provider_state
    ON provider_models(provider_id, presence, lifecycle_status, selection_policy);

CREATE INDEX idx_provider_models_provider_name
    ON provider_models(provider_id, name);

CREATE TABLE provider_model_cost_rules (
    provider_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    rule_index INTEGER NOT NULL CHECK (rule_index >= 0),
    rule_kind TEXT NOT NULL CHECK (rule_kind IN ('context_over_200k', 'tier')),
    threshold_tokens BIGINT NOT NULL CHECK (threshold_tokens >= 0),
    cost_input NUMERIC,
    cost_output NUMERIC,
    cost_reasoning NUMERIC,
    cost_cache_read NUMERIC,
    cost_cache_write NUMERIC,
    cost_input_audio NUMERIC,
    cost_output_audio NUMERIC,
    PRIMARY KEY (provider_id, model_id, rule_index),
    FOREIGN KEY (provider_id, model_id)
        REFERENCES provider_models(provider_id, model_id)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_provider_model_cost_rules_threshold
    ON provider_model_cost_rules(provider_id, model_id, rule_kind, threshold_tokens);
