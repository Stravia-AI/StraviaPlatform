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
    attachment INTEGER CHECK (attachment IS NULL OR attachment IN (0, 1)),
    reasoning INTEGER CHECK (reasoning IS NULL OR reasoning IN (0, 1)),
    tool_call INTEGER CHECK (tool_call IS NULL OR tool_call IN (0, 1)),
    open_weights INTEGER CHECK (open_weights IS NULL OR open_weights IN (0, 1)),
    structured_output INTEGER CHECK (structured_output IS NULL OR structured_output IN (0, 1)),
    temperature INTEGER CHECK (temperature IS NULL OR temperature IN (0, 1)),
    limit_context INTEGER CHECK (limit_context IS NULL OR limit_context >= 0),
    limit_input INTEGER CHECK (limit_input IS NULL OR limit_input >= 0),
    limit_output INTEGER CHECK (limit_output IS NULL OR limit_output >= 0),
    cost_input TEXT,
    cost_output TEXT,
    cost_reasoning TEXT,
    cost_cache_read TEXT,
    cost_cache_write TEXT,
    cost_input_audio TEXT,
    cost_output_audio TEXT,
    metadata_json TEXT NOT NULL
        CHECK (json_valid(metadata_json) AND json_type(metadata_json) = 'object'),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
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
    threshold_tokens INTEGER NOT NULL CHECK (threshold_tokens >= 0),
    cost_input TEXT,
    cost_output TEXT,
    cost_reasoning TEXT,
    cost_cache_read TEXT,
    cost_cache_write TEXT,
    cost_input_audio TEXT,
    cost_output_audio TEXT,
    PRIMARY KEY (provider_id, model_id, rule_index),
    FOREIGN KEY (provider_id, model_id)
        REFERENCES provider_models(provider_id, model_id)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_provider_model_cost_rules_threshold
    ON provider_model_cost_rules(provider_id, model_id, rule_kind, threshold_tokens);
