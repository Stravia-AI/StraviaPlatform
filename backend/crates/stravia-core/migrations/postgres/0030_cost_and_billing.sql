-- Cost and billing facts. Every amount is a bigint nanos column (1e-9 units).
-- Large tables only gain nullable or constant-default columns without CHECKs.

ALTER TABLE target_attempt_observations ADD COLUMN cost_basis TEXT;
ALTER TABLE target_attempt_observations ADD COLUMN cost_usd_nanos BIGINT;
ALTER TABLE target_attempt_observations ADD COLUMN cost_custom_nanos BIGINT;
ALTER TABLE target_attempt_observations ADD COLUMN billing_state TEXT;
ALTER TABLE target_attempt_observations ADD COLUMN billing_usd_nanos BIGINT;
ALTER TABLE target_attempt_observations ADD COLUMN billing_custom_nanos BIGINT;

ALTER TABLE stats_usage_buckets ADD COLUMN cost_custom_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN cost_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN cost_known_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN cost_unknown_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN payg_cost_custom_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN payg_cost_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN payg_cost_known_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN payg_cost_unknown_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN billing_custom_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN billing_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN billing_charged_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN billing_unknown_count BIGINT NOT NULL DEFAULT 0;

ALTER TABLE stats_usage_checkpoints ADD COLUMN cost_custom_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN cost_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN cost_known_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN cost_unknown_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN payg_cost_custom_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN payg_cost_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN payg_cost_known_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN payg_cost_unknown_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN billing_custom_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN billing_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN billing_charged_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN billing_unknown_count BIGINT NOT NULL DEFAULT 0;

ALTER TABLE api_keys ADD COLUMN spend_limit_nanos BIGINT
    CHECK (spend_limit_nanos IS NULL OR spend_limit_nanos > 0);

ALTER TABLE models ADD COLUMN billing_json JSONB
    CHECK (billing_json IS NULL OR jsonb_typeof(billing_json) = 'object');

ALTER TABLE provider_models ADD COLUMN source_metadata_json JSONB
    CHECK (source_metadata_json IS NULL OR jsonb_typeof(source_metadata_json) = 'object');

-- Append-only effective history; no Provider foreign key so deletion keeps history.
CREATE TABLE provider_cost_plans (
    id TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    effective_from BIGINT NOT NULL,
    basis TEXT NOT NULL,
    period TEXT,
    price_usd_nanos BIGINT,
    price_custom_nanos BIGINT,
    price_currency TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX provider_cost_plans_effective
    ON provider_cost_plans (provider_id, effective_from);

-- One cumulative row per API Key. Detail deletion and resets never rewrite it.
CREATE TABLE api_key_spend (
    api_key_id TEXT PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
    spent_custom_nanos BIGINT NOT NULL DEFAULT 0,
    spent_usd_unconverted_nanos BIGINT NOT NULL DEFAULT 0,
    reset_at TEXT,
    updated_at TEXT NOT NULL
);
