-- Keep the frozen baseline intact; existing values are validated atomically by ADD CONSTRAINT.
ALTER TABLE provider_models
    ADD COLUMN snapshot_state jsonb NOT NULL DEFAULT '{"type":"edited","source":null}'::jsonb,
    ADD CONSTRAINT provider_models_snapshot_state_shape CHECK (
        jsonb_typeof(snapshot_state) = 'object' AND CASE snapshot_state->>'type'
            WHEN 'unregistered' THEN NOT snapshot_state ? 'source'
            WHEN 'imported' THEN COALESCE(
                jsonb_typeof(snapshot_state->'source') = 'object' AND
                CASE snapshot_state->'source'->>'type'
                    WHEN 'provider_catalog' THEN jsonb_typeof(snapshot_state->'source'->'provider_id') = 'string'
                    WHEN 'canonical' THEN jsonb_typeof(snapshot_state->'source'->'model_id') = 'string'
                    WHEN 'discovery' THEN true ELSE false END, false)
            WHEN 'edited' THEN COALESCE(snapshot_state->'source' = 'null'::jsonb, false) OR COALESCE(
                jsonb_typeof(snapshot_state->'source') = 'object' AND
                CASE snapshot_state->'source'->>'type'
                    WHEN 'provider_catalog' THEN jsonb_typeof(snapshot_state->'source'->'provider_id') = 'string'
                    WHEN 'canonical' THEN jsonb_typeof(snapshot_state->'source'->'model_id') = 'string'
                    WHEN 'discovery' THEN true ELSE false END, false)
            ELSE false END
    );

ALTER TABLE models
    ADD CONSTRAINT models_balance_contract CHECK (balance IN ('traffic_equalization', 'latency_preference')),
    ADD CONSTRAINT models_default_thinking_level_contract CHECK (
        default_thinking_level IS NULL OR default_thinking_level IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max')
    );

ALTER TABLE model_backends
    ADD CONSTRAINT model_backends_priority_contract CHECK (priority IS NOT NULL),
    ADD CONSTRAINT model_backends_retry_contract CHECK (target_retry_budget >= 0),
    ADD CONSTRAINT model_backends_first_token_timeout_contract CHECK (first_token_timeout_ms >= 0),
    ADD CONSTRAINT model_backends_cooldown_contract CHECK (target_cooldown_ms >= 0),
    ADD CONSTRAINT model_backends_thinking_map_contract CHECK (jsonb_typeof(thinking_level_map) = 'array');

ALTER TABLE providers
    ADD CONSTRAINT providers_adapter_credentials_contract CHECK (
        adapter_credentials IS NOT NULL AND jsonb_typeof(adapter_credentials::jsonb) = 'object'
    ),
    ADD CONSTRAINT providers_vendor_options_contract CHECK (
        vendor_options IS NOT NULL AND jsonb_typeof(vendor_options::jsonb) = 'object'
    );

ALTER TABLE web_providers
    ADD CONSTRAINT web_providers_local_engines_contract CHECK (
        kind <> 'local' OR COALESCE(jsonb_typeof(local_engines) = 'object', false)
    );

ALTER TABLE inference_run_observations
    ADD CONSTRAINT inference_run_background_active_contract CHECK (background_active >= 0);

-- The existing single-column parent FK cannot enforce ownership or kind. The
-- composite key prevents a node from pointing at another principal's chain.
ALTER TABLE turn_chain_nodes
    ADD CONSTRAINT turn_chain_nodes_id_principal_kind_key UNIQUE (id, principal, kind);
ALTER TABLE turn_chain_nodes
    DROP CONSTRAINT turn_chain_nodes_parent_id_fkey,
    ADD CONSTRAINT turn_chain_nodes_parent_principal_kind_fkey
        FOREIGN KEY (parent_id, principal, kind)
        REFERENCES turn_chain_nodes(id, principal, kind) ON DELETE RESTRICT;
