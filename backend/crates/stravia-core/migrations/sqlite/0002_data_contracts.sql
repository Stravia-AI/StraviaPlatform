-- Startup runs this migration on a pinned connection with foreign_keys=OFF.
-- SQLx wraps this script and its history write in one transaction; failed
-- preflight and failed reconstruction leave the original tables unchanged.
CREATE TABLE _data_contract_guard (valid INTEGER NOT NULL CHECK (valid = 1));
INSERT INTO _data_contract_guard SELECT 0 FROM models
WHERE balance NOT IN ('traffic_equalization', 'latency_preference')
   OR (default_thinking_level IS NOT NULL AND default_thinking_level NOT IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'))
   OR typeof(is_enabled) <> 'integer' OR is_enabled NOT IN (0, 1) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM model_backends
WHERE priority IS NULL OR typeof(priority) <> 'integer' OR priority < -2147483648 OR priority > 2147483647
   OR typeof(target_retry_budget) <> 'integer' OR target_retry_budget < 0 OR target_retry_budget > 2147483647
   OR typeof(first_token_timeout_ms) <> 'integer' OR first_token_timeout_ms < 0
   OR typeof(target_cooldown_ms) <> 'integer' OR target_cooldown_ms < 0
   OR typeof(enabled) <> 'integer' OR enabled NOT IN (0, 1)
   OR CASE WHEN json_valid(thinking_level_map) THEN json_type(thinking_level_map) <> 'array' ELSE 1 END LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM providers
WHERE CASE WHEN json_valid(adapter_credentials) THEN json_type(adapter_credentials) <> 'object' ELSE 1 END
   OR CASE WHEN json_valid(vendor_options) THEN json_type(vendor_options) <> 'object' ELSE 1 END
   OR typeof(use_proxy) <> 'integer' OR use_proxy NOT IN (0, 1)
   OR typeof(is_enabled) <> 'integer' OR is_enabled NOT IN (0, 1) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM inference_run_observations
WHERE typeof(background_active) <> 'integer' OR background_active < 0 LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM turn_chain_nodes AS child
WHERE child.parent_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM turn_chain_nodes AS parent
    WHERE parent.id = child.parent_id AND parent.principal = child.principal AND parent.kind = child.kind
) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM api_keys
WHERE (typeof(is_enabled) <> 'integer' OR is_enabled NOT IN (0, 1)) OR (typeof(mcp_access_enabled) <> 'integer' OR mcp_access_enabled NOT IN (0, 1)) OR (typeof(transparent_injection_enabled) <> 'integer' OR transparent_injection_enabled NOT IN (0, 1)) OR (typeof(inject_media_understanding) <> 'integer' OR inject_media_understanding NOT IN (0, 1)) OR (typeof(inject_web_search) <> 'integer' OR inject_web_search NOT IN (0, 1)) OR (typeof(inject_media_generation) <> 'integer' OR inject_media_generation NOT IN (0, 1)) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM debug_trace_manifests
WHERE (typeof(tombstoned) <> 'integer' OR tombstoned NOT IN (0, 1)) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM inference_run_observations
WHERE (typeof(user_interrupted) <> 'integer' OR user_interrupted NOT IN (0, 1)) OR (typeof(debug_enabled) <> 'integer' OR debug_enabled NOT IN (0, 1)) OR (typeof(client_output_committed) <> 'integer' OR client_output_committed NOT IN (0, 1)) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM interaction_observations
WHERE (typeof(observation_gap) <> 'integer' OR observation_gap NOT IN (0, 1)) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM providers
WHERE (last_test_success IS NOT NULL AND (typeof(last_test_success) <> 'integer' OR last_test_success NOT IN (0, 1))) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM rejected_request_observations
WHERE (typeof(debug_enabled) <> 'integer' OR debug_enabled NOT IN (0, 1)) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM target_attempt_observations
WHERE (typeof(usage_recorded) <> 'integer' OR usage_recorded NOT IN (0, 1)) LIMIT 1;
INSERT INTO _data_contract_guard SELECT 0 FROM web_providers
WHERE (typeof(use_proxy) <> 'integer' OR use_proxy NOT IN (0, 1)) OR (last_test_success IS NOT NULL AND (typeof(last_test_success) <> 'integer' OR last_test_success NOT IN (0, 1)))
   OR (kind = 'local' AND CASE WHEN json_valid(local_engines) THEN json_type(local_engines) <> 'object' ELSE 1 END) LIMIT 1;

ALTER TABLE provider_models ADD COLUMN snapshot_state TEXT NOT NULL
    DEFAULT '{"type":"edited","source":null}'
    CHECK (COALESCE(CASE WHEN json_valid(snapshot_state) THEN
        json_type(snapshot_state) = 'object'
        AND CASE json_extract(snapshot_state, '$.type')
            WHEN 'unregistered' THEN json_type(snapshot_state, '$.source') IS NULL
            WHEN 'imported' THEN json_type(snapshot_state, '$.source') = 'object'
                AND CASE json_extract(snapshot_state, '$.source.type')
                    WHEN 'provider_catalog' THEN json_type(snapshot_state, '$.source.provider_id') = 'text'
                    WHEN 'canonical' THEN json_type(snapshot_state, '$.source.model_id') = 'text'
                    WHEN 'discovery' THEN 1 ELSE 0 END
            WHEN 'edited' THEN json_type(snapshot_state, '$.source') = 'null'
                OR (json_type(snapshot_state, '$.source') = 'object'
                    AND CASE json_extract(snapshot_state, '$.source.type')
                        WHEN 'provider_catalog' THEN json_type(snapshot_state, '$.source.provider_id') = 'text'
                        WHEN 'canonical' THEN json_type(snapshot_state, '$.source.model_id') = 'text'
                        WHEN 'discovery' THEN 1 ELSE 0 END)
            ELSE 0 END
        ELSE 0 END, 0));

-- The old single-column self FK is replaced, not supplemented. Rebuild by
-- copying first and dropping last, avoiding ON DELETE actions on child refs.
CREATE TABLE turn_chain_nodes_new (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('response', 'agent', 'web_search')),
    parent_id TEXT,
    principal TEXT NOT NULL,
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    prefix_namespace TEXT,
    prefix_fingerprint TEXT,
    prefix_item_count INTEGER,
    prefix_completed_at INTEGER,
    storage_format INTEGER NOT NULL DEFAULT 0 CHECK (storage_format IN (0, 1)),
    UNIQUE (id, principal),
    UNIQUE (id, principal, kind),
    FOREIGN KEY (parent_id, principal, kind)
        REFERENCES turn_chain_nodes_new(id, principal, kind) ON DELETE RESTRICT
);
INSERT INTO turn_chain_nodes_new (
    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at,
    prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at, storage_format
) SELECT
    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at,
    prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at, storage_format
FROM turn_chain_nodes;
DROP TABLE turn_chain_nodes;
ALTER TABLE turn_chain_nodes_new RENAME TO turn_chain_nodes;
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes(expires_at);
CREATE UNIQUE INDEX idx_turn_chain_node_principal ON turn_chain_nodes(id, principal);
CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes(principal, kind);
CREATE INDEX idx_turn_chain_reusable_prefix ON turn_chain_nodes (
    principal, kind, prefix_namespace, prefix_fingerprint, prefix_item_count DESC,
    prefix_completed_at DESC, expires_at, id DESC
) WHERE prefix_namespace IS NOT NULL;
-- Table-valued PRAGMA makes FK validation part of SQLx's migration transaction.
INSERT INTO _data_contract_guard SELECT 0 FROM pragma_foreign_key_check LIMIT 1;
DROP TABLE _data_contract_guard;

CREATE TRIGGER models_contract_insert BEFORE INSERT ON models
WHEN NEW.balance NOT IN ('traffic_equalization', 'latency_preference')
    OR (NEW.default_thinking_level IS NOT NULL AND NEW.default_thinking_level NOT IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'))
    OR typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)
BEGIN SELECT RAISE(ABORT, 'invalid Route configuration'); END;
CREATE TRIGGER models_contract_update BEFORE UPDATE ON models
WHEN NEW.balance NOT IN ('traffic_equalization', 'latency_preference')
    OR (NEW.default_thinking_level IS NOT NULL AND NEW.default_thinking_level NOT IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'))
    OR typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)
BEGIN SELECT RAISE(ABORT, 'invalid Route configuration'); END;
CREATE TRIGGER model_backends_contract_insert BEFORE INSERT ON model_backends
WHEN NEW.priority IS NULL OR typeof(NEW.priority) <> 'integer' OR NEW.priority < -2147483648 OR NEW.priority > 2147483647
    OR typeof(NEW.target_retry_budget) <> 'integer' OR NEW.target_retry_budget < 0 OR NEW.target_retry_budget > 2147483647
    OR typeof(NEW.first_token_timeout_ms) <> 'integer' OR NEW.first_token_timeout_ms < 0
    OR typeof(NEW.target_cooldown_ms) <> 'integer' OR NEW.target_cooldown_ms < 0
    OR typeof(NEW.enabled) <> 'integer' OR NEW.enabled NOT IN (0, 1)
    OR CASE WHEN json_valid(NEW.thinking_level_map) THEN json_type(NEW.thinking_level_map) <> 'array' ELSE 1 END
BEGIN SELECT RAISE(ABORT, 'invalid Target configuration'); END;
CREATE TRIGGER model_backends_contract_update BEFORE UPDATE ON model_backends
WHEN NEW.priority IS NULL OR typeof(NEW.priority) <> 'integer' OR NEW.priority < -2147483648 OR NEW.priority > 2147483647
    OR typeof(NEW.target_retry_budget) <> 'integer' OR NEW.target_retry_budget < 0 OR NEW.target_retry_budget > 2147483647
    OR typeof(NEW.first_token_timeout_ms) <> 'integer' OR NEW.first_token_timeout_ms < 0
    OR typeof(NEW.target_cooldown_ms) <> 'integer' OR NEW.target_cooldown_ms < 0
    OR typeof(NEW.enabled) <> 'integer' OR NEW.enabled NOT IN (0, 1)
    OR CASE WHEN json_valid(NEW.thinking_level_map) THEN json_type(NEW.thinking_level_map) <> 'array' ELSE 1 END
BEGIN SELECT RAISE(ABORT, 'invalid Target configuration'); END;
CREATE TRIGGER providers_contract_insert BEFORE INSERT ON providers
WHEN CASE WHEN json_valid(NEW.adapter_credentials) THEN json_type(NEW.adapter_credentials) <> 'object' ELSE 1 END
    OR CASE WHEN json_valid(NEW.vendor_options) THEN json_type(NEW.vendor_options) <> 'object' ELSE 1 END
    OR typeof(NEW.use_proxy) <> 'integer' OR NEW.use_proxy NOT IN (0, 1)
    OR typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)
BEGIN SELECT RAISE(ABORT, 'invalid Provider configuration'); END;
CREATE TRIGGER providers_contract_update BEFORE UPDATE ON providers
WHEN CASE WHEN json_valid(NEW.adapter_credentials) THEN json_type(NEW.adapter_credentials) <> 'object' ELSE 1 END
    OR CASE WHEN json_valid(NEW.vendor_options) THEN json_type(NEW.vendor_options) <> 'object' ELSE 1 END
    OR typeof(NEW.use_proxy) <> 'integer' OR NEW.use_proxy NOT IN (0, 1)
    OR typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)
BEGIN SELECT RAISE(ABORT, 'invalid Provider configuration'); END;
CREATE TRIGGER inference_runs_contract_insert BEFORE INSERT ON inference_run_observations
WHEN typeof(NEW.background_active) <> 'integer' OR NEW.background_active < 0
BEGIN SELECT RAISE(ABORT, 'invalid background activity count'); END;
CREATE TRIGGER inference_runs_contract_update BEFORE UPDATE ON inference_run_observations
WHEN typeof(NEW.background_active) <> 'integer' OR NEW.background_active < 0
BEGIN SELECT RAISE(ABORT, 'invalid background activity count'); END;

-- Enforce SQLite boolean storage classes, not just integer affinity.
CREATE TRIGGER api_keys_boolean_insert BEFORE INSERT ON api_keys
WHEN (typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)) OR (typeof(NEW.mcp_access_enabled) <> 'integer' OR NEW.mcp_access_enabled NOT IN (0, 1)) OR (typeof(NEW.transparent_injection_enabled) <> 'integer' OR NEW.transparent_injection_enabled NOT IN (0, 1)) OR (typeof(NEW.inject_media_understanding) <> 'integer' OR NEW.inject_media_understanding NOT IN (0, 1)) OR (typeof(NEW.inject_web_search) <> 'integer' OR NEW.inject_web_search NOT IN (0, 1)) OR (typeof(NEW.inject_media_generation) <> 'integer' OR NEW.inject_media_generation NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER api_keys_boolean_update BEFORE UPDATE ON api_keys
WHEN (typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)) OR (typeof(NEW.mcp_access_enabled) <> 'integer' OR NEW.mcp_access_enabled NOT IN (0, 1)) OR (typeof(NEW.transparent_injection_enabled) <> 'integer' OR NEW.transparent_injection_enabled NOT IN (0, 1)) OR (typeof(NEW.inject_media_understanding) <> 'integer' OR NEW.inject_media_understanding NOT IN (0, 1)) OR (typeof(NEW.inject_web_search) <> 'integer' OR NEW.inject_web_search NOT IN (0, 1)) OR (typeof(NEW.inject_media_generation) <> 'integer' OR NEW.inject_media_generation NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER debug_trace_manifests_boolean_insert BEFORE INSERT ON debug_trace_manifests
WHEN (typeof(NEW.tombstoned) <> 'integer' OR NEW.tombstoned NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER debug_trace_manifests_boolean_update BEFORE UPDATE ON debug_trace_manifests
WHEN (typeof(NEW.tombstoned) <> 'integer' OR NEW.tombstoned NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER inference_run_observations_boolean_insert BEFORE INSERT ON inference_run_observations
WHEN (typeof(NEW.user_interrupted) <> 'integer' OR NEW.user_interrupted NOT IN (0, 1)) OR (typeof(NEW.debug_enabled) <> 'integer' OR NEW.debug_enabled NOT IN (0, 1)) OR (typeof(NEW.client_output_committed) <> 'integer' OR NEW.client_output_committed NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER inference_run_observations_boolean_update BEFORE UPDATE ON inference_run_observations
WHEN (typeof(NEW.user_interrupted) <> 'integer' OR NEW.user_interrupted NOT IN (0, 1)) OR (typeof(NEW.debug_enabled) <> 'integer' OR NEW.debug_enabled NOT IN (0, 1)) OR (typeof(NEW.client_output_committed) <> 'integer' OR NEW.client_output_committed NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER interaction_observations_boolean_insert BEFORE INSERT ON interaction_observations
WHEN (typeof(NEW.observation_gap) <> 'integer' OR NEW.observation_gap NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER interaction_observations_boolean_update BEFORE UPDATE ON interaction_observations
WHEN (typeof(NEW.observation_gap) <> 'integer' OR NEW.observation_gap NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER providers_boolean_insert BEFORE INSERT ON providers
WHEN (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER providers_boolean_update BEFORE UPDATE ON providers
WHEN (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER rejected_request_observations_boolean_insert BEFORE INSERT ON rejected_request_observations
WHEN (typeof(NEW.debug_enabled) <> 'integer' OR NEW.debug_enabled NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER rejected_request_observations_boolean_update BEFORE UPDATE ON rejected_request_observations
WHEN (typeof(NEW.debug_enabled) <> 'integer' OR NEW.debug_enabled NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER target_attempt_observations_boolean_insert BEFORE INSERT ON target_attempt_observations
WHEN (typeof(NEW.usage_recorded) <> 'integer' OR NEW.usage_recorded NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER target_attempt_observations_boolean_update BEFORE UPDATE ON target_attempt_observations
WHEN (typeof(NEW.usage_recorded) <> 'integer' OR NEW.usage_recorded NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
CREATE TRIGGER web_providers_boolean_insert BEFORE INSERT ON web_providers
WHEN (typeof(NEW.use_proxy) <> 'integer' OR NEW.use_proxy NOT IN (0, 1)) OR (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
    OR (NEW.kind = 'local' AND CASE WHEN json_valid(NEW.local_engines) THEN json_type(NEW.local_engines) <> 'object' ELSE 1 END)
BEGIN SELECT RAISE(ABORT, 'invalid boolean or local engine configuration'); END;
CREATE TRIGGER web_providers_boolean_update BEFORE UPDATE ON web_providers
WHEN (typeof(NEW.use_proxy) <> 'integer' OR NEW.use_proxy NOT IN (0, 1)) OR (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
    OR (NEW.kind = 'local' AND CASE WHEN json_valid(NEW.local_engines) THEN json_type(NEW.local_engines) <> 'object' ELSE 1 END)
BEGIN SELECT RAISE(ABORT, 'invalid boolean or local engine configuration'); END;
