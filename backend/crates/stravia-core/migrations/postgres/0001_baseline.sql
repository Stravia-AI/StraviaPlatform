-- Current baseline schema. Applied only to empty databases; databases
-- carrying an older migration history are rejected at startup.

CREATE TABLE admin_identity (
    singleton_id smallint NOT NULL,
    username text,
    password_hash text,
    jwt_secret text NOT NULL,
    credential_revision bigint DEFAULT 1 NOT NULL,
    CONSTRAINT admin_identity_check CHECK (((username IS NULL) = (password_hash IS NULL))),
    CONSTRAINT admin_identity_credential_revision_check CHECK ((credential_revision > 0)),
    CONSTRAINT admin_identity_singleton_id_check CHECK ((singleton_id = 1))
);
CREATE TABLE admin_sessions (
    id text NOT NULL,
    identity_id smallint DEFAULT 1 NOT NULL,
    credential_revision bigint NOT NULL,
    refresh_hash text NOT NULL,
    expires_at bigint NOT NULL,
    revoked boolean DEFAULT false NOT NULL,
    CONSTRAINT admin_sessions_identity_id_check CHECK ((identity_id = 1))
);
CREATE TABLE agent_definition_configs (
    definition_id text NOT NULL,
    enabled boolean DEFAULT false NOT NULL,
    model_id text,
    updated_at bigint NOT NULL,
    thinking_level text,
    CONSTRAINT agent_definition_configs_thinking_level_check CHECK ((thinking_level = ANY (ARRAY['off'::text, 'minimal'::text, 'low'::text, 'medium'::text, 'high'::text, 'xhigh'::text, 'max'::text])))
);
CREATE TABLE agent_definition_revisions (
    definition_id text NOT NULL,
    slug text NOT NULL,
    version bigint NOT NULL,
    spec_hash text NOT NULL,
    spec_json text NOT NULL,
    created_at bigint NOT NULL,
    CONSTRAINT agent_definition_revisions_version_check CHECK ((version > 0))
);
CREATE TABLE api_key_models (
    api_key_id text NOT NULL,
    model_id text NOT NULL
);
CREATE TABLE api_keys (
    id text NOT NULL,
    token text NOT NULL,
    name text NOT NULL,
    is_enabled boolean DEFAULT true NOT NULL,
    expires_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    mcp_access_enabled boolean DEFAULT false CONSTRAINT api_keys_web_access_enabled_not_null NOT NULL,
    concurrency_limit integer,
    transparent_injection_enabled boolean DEFAULT false NOT NULL,
    inject_media_understanding boolean DEFAULT false NOT NULL,
    inject_web_search boolean DEFAULT false NOT NULL,
    inject_media_generation boolean DEFAULT false NOT NULL,
    CONSTRAINT api_keys_concurrency_limit_check CHECK ((concurrency_limit > 0))
);
CREATE TABLE artifact_download_grants (
    token_hash text NOT NULL,
    artifact_id text NOT NULL,
    expires_at bigint NOT NULL
);
CREATE TABLE artifact_upload_parts (
    upload_id text NOT NULL,
    part_number bigint NOT NULL,
    etag text NOT NULL,
    size bigint NOT NULL,
    CONSTRAINT artifact_upload_parts_part_number_check CHECK ((part_number > 0)),
    CONSTRAINT artifact_upload_parts_size_check CHECK ((size >= 0))
);
CREATE TABLE artifact_uploads (
    id text NOT NULL,
    artifact_id text NOT NULL,
    principal text NOT NULL,
    token_hash text NOT NULL,
    declared_size bigint NOT NULL,
    received_size bigint DEFAULT 0 NOT NULL,
    expires_at bigint NOT NULL,
    created_at bigint NOT NULL,
    CONSTRAINT artifact_uploads_declared_size_check CHECK ((declared_size >= 0)),
    CONSTRAINT artifact_uploads_received_size_check CHECK ((received_size >= 0))
);
CREATE TABLE artifacts (
    id text NOT NULL,
    principal text NOT NULL,
    mime_type text NOT NULL,
    size bigint NOT NULL,
    backend_key text NOT NULL,
    state text NOT NULL,
    expires_at bigint NOT NULL,
    created_at bigint NOT NULL,
    storage_backend text DEFAULT 'internal'::text NOT NULL,
    storage_endpoint text,
    storage_bucket text,
    CONSTRAINT artifacts_size_check CHECK ((size >= 0)),
    CONSTRAINT artifacts_state_check CHECK ((state = ANY (ARRAY['staging'::text, 'ready'::text])))
);
CREATE TABLE debug_trace_manifests (
    trace_id text NOT NULL,
    run_id text,
    rejection_id text,
    relative_directory text NOT NULL,
    bytes_written bigint DEFAULT 0 NOT NULL,
    event_count bigint DEFAULT 0 NOT NULL,
    status text NOT NULL,
    partial_reason text,
    tombstoned boolean DEFAULT false NOT NULL,
    created_at bigint NOT NULL,
    completed_at bigint,
    expires_at bigint NOT NULL,
    CONSTRAINT debug_trace_manifests_check CHECK (((run_id IS NOT NULL) <> (rejection_id IS NOT NULL)))
);
CREATE TABLE history_markers (
    reference text NOT NULL,
    principal text NOT NULL,
    kind text NOT NULL,
    activity text NOT NULL,
    tool_id text,
    call_payload text,
    segment_payload text,
    execution_state text,
    execution_owner text,
    lease_expires_at bigint,
    execution_deadline bigint,
    published_at bigint,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    expires_at bigint NOT NULL,
    CONSTRAINT history_markers_check CHECK ((((kind = 'thinking'::text) AND (tool_id IS NULL) AND (call_payload IS NULL) AND (execution_state IS NULL) AND (segment_payload IS NOT NULL)) OR ((kind = 'platform'::text) AND (tool_id IS NOT NULL) AND (call_payload IS NOT NULL) AND (execution_state IS NOT NULL) AND (execution_deadline IS NOT NULL)))),
    CONSTRAINT history_markers_execution_state_check CHECK (((execution_state IS NULL) OR (execution_state = ANY (ARRAY['pending'::text, 'running'::text, 'completed'::text, 'failed'::text, 'interrupted'::text])))),
    CONSTRAINT history_markers_kind_check CHECK ((kind = ANY (ARRAY['platform'::text, 'thinking'::text])))
);
CREATE TABLE inference_run_observations (
    id text NOT NULL,
    interaction_id text NOT NULL,
    parent_run_id text,
    generation_node_id text,
    generation_parent_id text,
    ingress_protocol text NOT NULL,
    route_id text NOT NULL,
    model_display_name text,
    status text NOT NULL,
    terminal_reason text,
    user_interrupted boolean DEFAULT false NOT NULL,
    background_active bigint DEFAULT 0 NOT NULL,
    debug_enabled boolean NOT NULL,
    client_output_committed boolean DEFAULT false NOT NULL,
    started_at bigint NOT NULL,
    last_active_at bigint NOT NULL,
    finished_at bigint,
    last_event_sequence bigint DEFAULT 0 NOT NULL,
    expires_at bigint NOT NULL,
    failure_json text,
    request_model text
);
CREATE TABLE interaction_observations (
    id text NOT NULL,
    principal text NOT NULL,
    api_key_id text,
    api_key_name text,
    generation_root_id text,
    parent_interaction_id text,
    root_id text NOT NULL,
    root_run_id text NOT NULL,
    first_route_id text NOT NULL,
    first_model_display_name text,
    status text NOT NULL,
    started_at bigint NOT NULL,
    last_active_at bigint NOT NULL,
    visible_tail text DEFAULT ''::text NOT NULL,
    input_tokens bigint,
    output_tokens bigint,
    cache_read_tokens bigint,
    cache_write_tokens bigint,
    reasoning_tokens bigint,
    observation_gap boolean DEFAULT false NOT NULL,
    last_event_sequence bigint DEFAULT 0 NOT NULL,
    expires_at bigint NOT NULL,
    input_preview text
);
CREATE TABLE media_derivatives (
    principal text NOT NULL,
    source_artifact_id text NOT NULL,
    derivative_artifact_id text NOT NULL,
    created_at bigint NOT NULL
);
CREATE TABLE model_backends (
    id text NOT NULL,
    model_id text NOT NULL,
    provider_id text NOT NULL,
    model text,
    priority integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    thinking_level_map jsonb DEFAULT '[{"level": "off", "source": "generated", "control": {"type": "hidden"}}, {"level": "minimal", "source": "generated", "control": {"type": "hidden"}}, {"level": "low", "source": "generated", "control": {"type": "hidden"}}, {"level": "medium", "source": "generated", "control": {"type": "hidden"}}, {"level": "high", "source": "generated", "control": {"type": "hidden"}}, {"level": "xhigh", "source": "generated", "control": {"type": "hidden"}}, {"level": "max", "source": "generated", "control": {"type": "hidden"}}]'::jsonb NOT NULL,
    first_token_timeout_ms bigint DEFAULT 60000 NOT NULL,
    target_retry_budget integer DEFAULT 5 NOT NULL,
    target_cooldown_ms bigint DEFAULT 120000 NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    CONSTRAINT model_backends_model_nonblank CHECK (((model IS NULL) OR (btrim(model) <> ''::text)))
);
CREATE TABLE model_turn_observations (
    id text NOT NULL,
    run_id text NOT NULL,
    interaction_id text NOT NULL,
    route_id text NOT NULL,
    model_display_name text,
    api_key_id text,
    api_key_name text,
    status text NOT NULL,
    started_at bigint NOT NULL,
    finished_at bigint,
    input_tokens bigint,
    output_tokens bigint,
    cache_read_tokens bigint,
    cache_write_tokens bigint,
    reasoning_tokens bigint,
    last_event_sequence bigint NOT NULL
);
CREATE TABLE models (
    id text NOT NULL,
    model_id text CONSTRAINT models_name_not_null NOT NULL,
    balance text DEFAULT 'traffic_equalization'::text NOT NULL,
    is_enabled boolean DEFAULT true NOT NULL,
    priority integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    display_name text,
    default_thinking_level text
);
CREATE TABLE native_compaction_sources (
    record_id text NOT NULL,
    source_id text NOT NULL,
    CONSTRAINT native_compaction_sources_check CHECK ((record_id <> source_id))
);
CREATE TABLE native_compaction_states (
    record_id text NOT NULL,
    principal text NOT NULL,
    native_identity text,
    fingerprint text NOT NULL,
    state_payload text NOT NULL
);
CREATE TABLE native_compactions (
    id text NOT NULL,
    principal text NOT NULL,
    source_generation_id text,
    operation_id text NOT NULL,
    payload text NOT NULL,
    created_at bigint NOT NULL,
    delivered_at bigint,
    referenced_at bigint,
    expires_at bigint NOT NULL
);
CREATE SEQUENCE observation_event_sequence
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
CREATE TABLE observation_events (
    sequence bigint DEFAULT nextval('observation_event_sequence'::regclass) NOT NULL,
    occurred_at bigint NOT NULL,
    interaction_id text,
    run_id text,
    rejection_id text,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    expires_at bigint NOT NULL
);
CREATE TABLE observation_pending_tools (
    principal text NOT NULL,
    tool_id text NOT NULL,
    run_id text NOT NULL,
    interaction_id text NOT NULL,
    expires_at bigint NOT NULL
);
CREATE TABLE observation_tail_sources (
    run_id text NOT NULL,
    interaction_id text NOT NULL,
    principal text NOT NULL,
    last_unit_hash text NOT NULL,
    generation_node_id text,
    expires_at bigint NOT NULL
);
CREATE TABLE provider_allowance_samples (
    id text NOT NULL,
    provider_id text NOT NULL,
    allowance_key text NOT NULL,
    sampled_at bigint NOT NULL,
    used_value double precision,
    remaining_value double precision,
    limit_value double precision,
    used_percent double precision,
    amount_unit text,
    currency text,
    reset_at bigint
);
CREATE TABLE provider_model_cost_rules (
    provider_id text NOT NULL,
    model_id text NOT NULL,
    rule_index integer NOT NULL,
    rule_kind text NOT NULL,
    threshold_tokens bigint NOT NULL,
    cost_input numeric,
    cost_output numeric,
    cost_reasoning numeric,
    cost_cache_read numeric,
    cost_cache_write numeric,
    cost_input_audio numeric,
    cost_output_audio numeric,
    CONSTRAINT provider_model_cost_rules_rule_index_check CHECK ((rule_index >= 0)),
    CONSTRAINT provider_model_cost_rules_rule_kind_check CHECK ((rule_kind = ANY (ARRAY['context_over_200k'::text, 'tier'::text]))),
    CONSTRAINT provider_model_cost_rules_threshold_tokens_check CHECK ((threshold_tokens >= 0))
);
CREATE TABLE provider_models (
    provider_id text NOT NULL,
    model_id text NOT NULL,
    source_kind text NOT NULL,
    metadata_source_provider_id text,
    presence text NOT NULL,
    lifecycle_status text,
    selection_policy text DEFAULT 'auto'::text NOT NULL,
    name text,
    family text,
    attachment boolean,
    reasoning boolean,
    tool_call boolean,
    open_weights boolean,
    structured_output boolean,
    temperature boolean,
    limit_context bigint,
    limit_input bigint,
    limit_output bigint,
    cost_input numeric,
    cost_output numeric,
    cost_reasoning numeric,
    cost_cache_read numeric,
    cost_cache_write numeric,
    cost_input_audio numeric,
    cost_output_audio numeric,
    metadata_json jsonb NOT NULL,
    revision bigint DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT provider_models_lifecycle_status_check CHECK ((lifecycle_status = ANY (ARRAY['alpha'::text, 'beta'::text, 'deprecated'::text]))),
    CONSTRAINT provider_models_limit_context_check CHECK (((limit_context IS NULL) OR (limit_context >= 0))),
    CONSTRAINT provider_models_limit_input_check CHECK (((limit_input IS NULL) OR (limit_input >= 0))),
    CONSTRAINT provider_models_limit_output_check CHECK (((limit_output IS NULL) OR (limit_output >= 0))),
    CONSTRAINT provider_models_metadata_json_check CHECK ((jsonb_typeof(metadata_json) = 'object'::text)),
    CONSTRAINT provider_models_presence_check CHECK ((presence = ANY (ARRAY['present'::text, 'missing'::text]))),
    CONSTRAINT provider_models_revision_check CHECK ((revision > 0)),
    CONSTRAINT provider_models_selection_policy_check CHECK ((selection_policy = ANY (ARRAY['auto'::text, 'force_enabled'::text, 'force_disabled'::text]))),
    CONSTRAINT provider_models_source_kind_check CHECK ((source_kind = ANY (ARRAY['discovered'::text, 'manual'::text])))
);
CREATE TABLE provider_oauth_credentials (
    provider_id text NOT NULL,
    driver_key text DEFAULT ''::text NOT NULL,
    scheme text DEFAULT ''::text NOT NULL,
    access_token text DEFAULT ''::text NOT NULL,
    refresh_token text,
    expires_at timestamp with time zone,
    resource_url text,
    subject_id text,
    scopes text DEFAULT '[]'::text NOT NULL,
    meta text DEFAULT '{}'::text NOT NULL,
    status text DEFAULT 'connected'::text NOT NULL,
    status_version integer DEFAULT 0 NOT NULL,
    last_error text,
    last_refresh_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP,
    connection_id text NOT NULL
);
CREATE TABLE providers (
    id text NOT NULL,
    name text NOT NULL,
    vendor text,
    protocol text NOT NULL,
    base_url text NOT NULL,
    preset_key text,
    channel text,
    models_source text,
    static_models text,
    api_key text NOT NULL,
    auth_mode text DEFAULT 'apikey'::text NOT NULL,
    access_token text,
    refresh_token text,
    expires_at timestamp with time zone,
    use_proxy boolean DEFAULT false NOT NULL,
    last_test_success boolean,
    last_test_at timestamp with time zone,
    is_enabled boolean DEFAULT true NOT NULL,
    priority integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    adapter_credentials text DEFAULT '{}'::text NOT NULL,
    vendor_options text DEFAULT '{}'::text NOT NULL,
    CONSTRAINT providers_auth_mode_check CHECK ((auth_mode = ANY (ARRAY['apikey'::text, 'oauth'::text])))
);
CREATE TABLE rejected_request_observations (
    id text NOT NULL,
    occurred_at bigint NOT NULL,
    method text NOT NULL,
    path text NOT NULL,
    ingress_protocol text NOT NULL,
    stage text NOT NULL,
    code text NOT NULL,
    status_code bigint NOT NULL,
    debug_enabled boolean NOT NULL,
    debug_status text NOT NULL,
    last_event_sequence bigint NOT NULL,
    expires_at bigint NOT NULL,
    failure_json text,
    request_model text,
    api_key_id text,
    api_key_name text,
    started_at bigint,
    duration_ms bigint
);
CREATE TABLE reversible_redaction_mappings (
    reference text NOT NULL,
    principal text NOT NULL,
    secret text NOT NULL,
    published_at bigint,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    expires_at bigint NOT NULL
);
CREATE TABLE settings (
    name text NOT NULL,
    value text NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE target_attempt_observations (
    id text NOT NULL,
    model_turn_id text NOT NULL,
    run_id text NOT NULL,
    interaction_id text NOT NULL,
    target_id text NOT NULL,
    provider_id text NOT NULL,
    provider_name text NOT NULL,
    upstream_model text NOT NULL,
    protocol text NOT NULL,
    status text NOT NULL,
    status_code bigint,
    error_code text,
    started_at bigint NOT NULL,
    finished_at bigint,
    duration_ms bigint,
    first_token_ms bigint,
    input_tokens bigint,
    output_tokens bigint,
    cache_read_tokens bigint,
    cache_write_tokens bigint,
    reasoning_tokens bigint,
    usage_recorded boolean DEFAULT false NOT NULL,
    last_event_sequence bigint NOT NULL
);
CREATE TABLE turn_chain_content_refs (
    node_id text NOT NULL,
    principal text NOT NULL,
    path text NOT NULL,
    content_key text NOT NULL
);
CREATE TABLE turn_chain_contents (
    principal text NOT NULL,
    content_key text NOT NULL,
    content text NOT NULL
);
CREATE TABLE turn_chain_nodes (
    id text NOT NULL,
    kind text NOT NULL,
    parent_id text,
    principal text NOT NULL,
    payload_version bigint NOT NULL,
    payload text NOT NULL,
    created_at bigint NOT NULL,
    expires_at bigint NOT NULL,
    prefix_namespace text,
    prefix_fingerprint text,
    prefix_item_count bigint,
    prefix_completed_at bigint,
    storage_format integer DEFAULT 0 NOT NULL,
    CONSTRAINT turn_chain_nodes_kind_check CHECK ((kind = ANY (ARRAY['response'::text, 'agent'::text, 'web_search'::text]))),
    CONSTRAINT turn_chain_nodes_payload_version_check CHECK ((payload_version > 0)),
    CONSTRAINT turn_chain_nodes_storage_format_check CHECK ((storage_format = ANY (ARRAY[0, 1])))
);
CREATE TABLE vendor_data_recovery (
    provider_id text NOT NULL,
    data_kind text NOT NULL,
    CONSTRAINT vendor_data_recovery_data_kind_check CHECK ((data_kind = ANY (ARRAY['options'::text, 'credentials'::text, 'models'::text])))
);
CREATE TABLE vendor_plugins (
    vendor_id text NOT NULL,
    version text NOT NULL,
    source text NOT NULL,
    descriptor text NOT NULL,
    digest text NOT NULL,
    revision bigint NOT NULL,
    data_epoch bigint NOT NULL,
    installed_at bigint NOT NULL,
    CONSTRAINT vendor_plugins_data_epoch_check CHECK ((data_epoch >= 0)),
    CONSTRAINT vendor_plugins_revision_check CHECK ((revision > 0)),
    CONSTRAINT vendor_plugins_source_check CHECK ((source = ANY (ARRAY['builtin'::text, 'local'::text])))
);
CREATE TABLE vendor_private_state (
    provider_id text NOT NULL,
    vendor_id text NOT NULL,
    format_version text NOT NULL,
    payload bytea NOT NULL,
    updated_at bigint NOT NULL,
    CONSTRAINT vendor_private_state_payload_check CHECK ((octet_length(payload) <= 262144))
);
CREATE TABLE web_providers (
    id text NOT NULL,
    name text NOT NULL,
    kind text NOT NULL,
    api_key text,
    last_test_success boolean,
    last_test_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    updated_at timestamp with time zone DEFAULT CURRENT_TIMESTAMP NOT NULL,
    use_proxy boolean DEFAULT false NOT NULL,
    local_engines jsonb,
    CONSTRAINT web_providers_credentials_check CHECK ((((kind = 'local'::text) AND (api_key IS NULL) AND (local_engines IS NOT NULL)) OR ((kind = ANY (ARRAY['exa'::text, 'zhipu'::text])) AND (api_key IS NOT NULL) AND (length(TRIM(BOTH FROM api_key)) > 0) AND (local_engines IS NULL)))),
    CONSTRAINT web_providers_kind_check CHECK ((kind = ANY (ARRAY['local'::text, 'exa'::text, 'zhipu'::text])))
);
ALTER TABLE ONLY admin_identity
    ADD CONSTRAINT admin_identity_pkey PRIMARY KEY (singleton_id);
ALTER TABLE ONLY admin_identity
    ADD CONSTRAINT admin_identity_username_key UNIQUE (username);
ALTER TABLE ONLY admin_sessions
    ADD CONSTRAINT admin_sessions_pkey PRIMARY KEY (id);
ALTER TABLE ONLY admin_sessions
    ADD CONSTRAINT admin_sessions_refresh_hash_key UNIQUE (refresh_hash);
ALTER TABLE ONLY agent_definition_configs
    ADD CONSTRAINT agent_definition_configs_pkey PRIMARY KEY (definition_id);
ALTER TABLE ONLY agent_definition_revisions
    ADD CONSTRAINT agent_definition_revisions_pkey PRIMARY KEY (definition_id, version);
ALTER TABLE ONLY agent_definition_revisions
    ADD CONSTRAINT agent_definition_revisions_slug_version_key UNIQUE (slug, version);
ALTER TABLE ONLY api_key_models
    ADD CONSTRAINT api_key_models_pkey PRIMARY KEY (api_key_id, model_id);
ALTER TABLE ONLY api_keys
    ADD CONSTRAINT api_keys_pkey PRIMARY KEY (id);
ALTER TABLE ONLY api_keys
    ADD CONSTRAINT api_keys_token_key UNIQUE (token);
ALTER TABLE ONLY artifact_download_grants
    ADD CONSTRAINT artifact_download_grants_pkey PRIMARY KEY (token_hash);
ALTER TABLE ONLY artifact_upload_parts
    ADD CONSTRAINT artifact_upload_parts_pkey PRIMARY KEY (upload_id, part_number);
ALTER TABLE ONLY artifact_uploads
    ADD CONSTRAINT artifact_uploads_pkey PRIMARY KEY (id);
ALTER TABLE ONLY artifacts
    ADD CONSTRAINT artifacts_pkey PRIMARY KEY (id);
ALTER TABLE ONLY debug_trace_manifests
    ADD CONSTRAINT debug_trace_manifests_pkey PRIMARY KEY (trace_id);
ALTER TABLE ONLY debug_trace_manifests
    ADD CONSTRAINT debug_trace_manifests_relative_directory_key UNIQUE (relative_directory);
ALTER TABLE ONLY history_markers
    ADD CONSTRAINT history_markers_pkey PRIMARY KEY (reference);
ALTER TABLE ONLY inference_run_observations
    ADD CONSTRAINT inference_run_observations_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interaction_observations
    ADD CONSTRAINT interaction_observations_pkey PRIMARY KEY (id);
ALTER TABLE ONLY media_derivatives
    ADD CONSTRAINT media_derivatives_pkey PRIMARY KEY (source_artifact_id);
ALTER TABLE ONLY model_backends
    ADD CONSTRAINT model_backends_pkey PRIMARY KEY (id);
ALTER TABLE ONLY model_turn_observations
    ADD CONSTRAINT model_turn_observations_pkey PRIMARY KEY (id);
ALTER TABLE ONLY models
    ADD CONSTRAINT models_pkey PRIMARY KEY (id);
ALTER TABLE ONLY native_compaction_sources
    ADD CONSTRAINT native_compaction_sources_pkey PRIMARY KEY (record_id, source_id);
ALTER TABLE ONLY native_compaction_states
    ADD CONSTRAINT native_compaction_states_pkey PRIMARY KEY (record_id, fingerprint);
ALTER TABLE ONLY native_compactions
    ADD CONSTRAINT native_compactions_pkey PRIMARY KEY (id);
ALTER TABLE ONLY observation_events
    ADD CONSTRAINT observation_events_pkey PRIMARY KEY (sequence);
ALTER TABLE ONLY observation_pending_tools
    ADD CONSTRAINT observation_pending_tools_pkey PRIMARY KEY (principal, tool_id, run_id);
ALTER TABLE ONLY observation_tail_sources
    ADD CONSTRAINT observation_tail_sources_pkey PRIMARY KEY (run_id);
ALTER TABLE ONLY provider_allowance_samples
    ADD CONSTRAINT provider_allowance_samples_pkey PRIMARY KEY (id);
ALTER TABLE ONLY provider_model_cost_rules
    ADD CONSTRAINT provider_model_cost_rules_pkey PRIMARY KEY (provider_id, model_id, rule_index);
ALTER TABLE ONLY provider_models
    ADD CONSTRAINT provider_models_pkey PRIMARY KEY (provider_id, model_id);
ALTER TABLE ONLY provider_oauth_credentials
    ADD CONSTRAINT provider_oauth_credentials_pkey PRIMARY KEY (provider_id);
ALTER TABLE ONLY providers
    ADD CONSTRAINT providers_pkey PRIMARY KEY (id);
ALTER TABLE ONLY rejected_request_observations
    ADD CONSTRAINT rejected_request_observations_pkey PRIMARY KEY (id);
ALTER TABLE ONLY reversible_redaction_mappings
    ADD CONSTRAINT reversible_redaction_mappings_pkey PRIMARY KEY (reference);
ALTER TABLE ONLY settings
    ADD CONSTRAINT settings_pkey PRIMARY KEY (name);
ALTER TABLE ONLY target_attempt_observations
    ADD CONSTRAINT target_attempt_observations_pkey PRIMARY KEY (id);
ALTER TABLE ONLY turn_chain_content_refs
    ADD CONSTRAINT turn_chain_content_refs_pkey PRIMARY KEY (node_id, path);
ALTER TABLE ONLY turn_chain_contents
    ADD CONSTRAINT turn_chain_contents_pkey PRIMARY KEY (principal, content_key);
ALTER TABLE ONLY turn_chain_nodes
    ADD CONSTRAINT turn_chain_nodes_pkey PRIMARY KEY (id);
ALTER TABLE ONLY vendor_data_recovery
    ADD CONSTRAINT vendor_data_recovery_pkey PRIMARY KEY (provider_id, data_kind);
ALTER TABLE ONLY vendor_plugins
    ADD CONSTRAINT vendor_plugins_pkey PRIMARY KEY (vendor_id);
ALTER TABLE ONLY vendor_private_state
    ADD CONSTRAINT vendor_private_state_pkey PRIMARY KEY (provider_id);
ALTER TABLE ONLY web_providers
    ADD CONSTRAINT web_providers_name_key UNIQUE (name);
ALTER TABLE ONLY web_providers
    ADD CONSTRAINT web_providers_pkey PRIMARY KEY (id);
CREATE INDEX debug_manifests_expiry_idx ON debug_trace_manifests USING btree (tombstoned, expires_at);
CREATE INDEX idx_admin_sessions_expiry ON admin_sessions USING btree (expires_at);
CREATE INDEX idx_agent_definition_revisions_slug ON agent_definition_revisions USING btree (slug, version);
CREATE INDEX idx_api_key_models_model_id ON api_key_models USING btree (model_id);
CREATE INDEX idx_api_keys_token ON api_keys USING btree (token);
CREATE INDEX idx_artifact_download_grants_hold ON artifact_download_grants USING btree (artifact_id, expires_at);
CREATE INDEX idx_artifact_uploads_expiry ON artifact_uploads USING btree (expires_at);
CREATE INDEX idx_artifacts_expiry ON artifacts USING btree (expires_at);
CREATE INDEX idx_history_markers_execution ON history_markers USING btree (execution_state, lease_expires_at, execution_deadline);
CREATE INDEX idx_history_markers_expiry ON history_markers USING btree (expires_at);
CREATE INDEX idx_history_markers_principal_reference ON history_markers USING btree (principal, reference);
CREATE INDEX idx_media_derivatives_derivative ON media_derivatives USING btree (derivative_artifact_id);
CREATE INDEX idx_model_backends_model_id ON model_backends USING btree (model_id);
CREATE UNIQUE INDEX idx_models_route_id ON models USING btree (model_id);
CREATE INDEX idx_native_compaction_source ON native_compaction_sources USING btree (source_id);
CREATE INDEX idx_native_compaction_state_fingerprint ON native_compaction_states USING btree (principal, fingerprint);
CREATE INDEX idx_native_compaction_state_identity ON native_compaction_states USING btree (principal, native_identity);
CREATE INDEX idx_native_compactions_expiry ON native_compactions USING btree (expires_at);
CREATE UNIQUE INDEX idx_oauth_creds_connection_id ON provider_oauth_credentials USING btree (connection_id);
CREATE INDEX idx_oauth_creds_expires ON provider_oauth_credentials USING btree (expires_at);
CREATE INDEX idx_oauth_creds_status ON provider_oauth_credentials USING btree (status);
CREATE INDEX idx_observation_events_client_tool_call ON observation_events USING btree (run_id, ((payload ->> 'tool_id'::text)), sequence DESC) WHERE (kind = ANY (ARRAY['client_tool_handoff'::text, 'client_tool_result'::text]));
CREATE INDEX idx_observation_events_context ON observation_events USING btree (interaction_id, sequence) WHERE (kind = ANY (ARRAY['compaction_operation'::text, 'native_compaction_associated'::text, 'retained_tail_associated'::text]));
CREATE INDEX idx_provider_allowance_samples_item_time ON provider_allowance_samples USING btree (provider_id, allowance_key, sampled_at);
CREATE INDEX idx_provider_allowance_samples_sampled_at ON provider_allowance_samples USING btree (sampled_at);
CREATE UNIQUE INDEX idx_provider_model_cost_rules_threshold ON provider_model_cost_rules USING btree (provider_id, model_id, rule_kind, threshold_tokens);
CREATE INDEX idx_provider_models_provider_name ON provider_models USING btree (provider_id, name);
CREATE INDEX idx_provider_models_provider_state ON provider_models USING btree (provider_id, presence, lifecycle_status, selection_policy);
CREATE INDEX idx_reversible_redaction_mappings_expiry ON reversible_redaction_mappings USING btree (expires_at);
CREATE INDEX idx_reversible_redaction_mappings_principal_expiry ON reversible_redaction_mappings USING btree (principal, expires_at);
CREATE INDEX idx_turn_chain_content_refs_content ON turn_chain_content_refs USING btree (principal, content_key);
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes USING btree (expires_at);
CREATE UNIQUE INDEX idx_turn_chain_node_principal ON turn_chain_nodes USING btree (id, principal);
CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes USING btree (parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes USING btree (principal, kind);
CREATE INDEX idx_turn_chain_reusable_prefix ON turn_chain_nodes USING btree (principal, kind, prefix_namespace, prefix_fingerprint, prefix_item_count DESC, prefix_completed_at DESC, expires_at, id DESC) WHERE (prefix_namespace IS NOT NULL);
CREATE INDEX idx_vendor_private_state_vendor ON vendor_private_state USING btree (vendor_id);
CREATE UNIQUE INDEX idx_web_providers_local_singleton ON web_providers USING btree (kind) WHERE (kind = 'local'::text);
CREATE INDEX inference_runs_failed_window_idx ON inference_run_observations USING btree (started_at DESC, id) WHERE (status = 'failed'::text);
CREATE INDEX inference_runs_generation_idx ON inference_run_observations USING btree (generation_node_id, generation_parent_id);
CREATE INDEX inference_runs_interaction_idx ON inference_run_observations USING btree (interaction_id, started_at, id);
CREATE INDEX inference_runs_status_idx ON inference_run_observations USING btree (status, last_active_at);
CREATE INDEX interaction_observations_expiry_idx ON interaction_observations USING btree (expires_at);
CREATE INDEX interaction_observations_filter_idx ON interaction_observations USING btree (status, api_key_name, last_active_at DESC);
CREATE INDEX interaction_observations_generation_idx ON interaction_observations USING btree (generation_root_id, last_active_at);
CREATE INDEX interaction_observations_window_idx ON interaction_observations USING btree (root_id, last_active_at DESC, id);
CREATE INDEX model_turns_analytics_idx ON model_turn_observations USING btree (started_at, route_id, api_key_id, status);
CREATE INDEX model_turns_interaction_idx ON model_turn_observations USING btree (interaction_id, started_at, id);
CREATE INDEX observation_events_expiry_idx ON observation_events USING btree (expires_at, sequence);
CREATE INDEX observation_events_interaction_idx ON observation_events USING btree (interaction_id, sequence);
CREATE INDEX observation_events_rejection_idx ON observation_events USING btree (rejection_id, sequence);
CREATE INDEX observation_events_run_idx ON observation_events USING btree (run_id, sequence);
CREATE INDEX observation_pending_tools_expiry_idx ON observation_pending_tools USING btree (expires_at);
CREATE INDEX observation_pending_tools_lookup_idx ON observation_pending_tools USING btree (principal, tool_id);
CREATE INDEX observation_tail_sources_expiry_idx ON observation_tail_sources USING btree (expires_at);
CREATE INDEX observation_tail_sources_hash_idx ON observation_tail_sources USING btree (principal, last_unit_hash);
CREATE INDEX rejected_requests_expiry_idx ON rejected_request_observations USING btree (expires_at);
CREATE INDEX rejected_requests_started_idx ON rejected_request_observations USING btree (started_at DESC, id);
CREATE INDEX rejected_requests_window_idx ON rejected_request_observations USING btree (occurred_at DESC, id);
CREATE INDEX target_attempts_analytics_idx ON target_attempt_observations USING btree (started_at, provider_id, upstream_model, target_id, status);
CREATE INDEX target_attempts_turn_idx ON target_attempt_observations USING btree (model_turn_id, started_at, id);
ALTER TABLE ONLY admin_sessions
    ADD CONSTRAINT admin_sessions_identity_id_fkey FOREIGN KEY (identity_id) REFERENCES admin_identity(singleton_id) ON DELETE CASCADE;
ALTER TABLE ONLY agent_definition_configs
    ADD CONSTRAINT agent_definition_configs_model_id_fkey FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE SET NULL;
ALTER TABLE ONLY api_key_models
    ADD CONSTRAINT api_key_models_api_key_id_fkey FOREIGN KEY (api_key_id) REFERENCES api_keys(id) ON DELETE CASCADE;
ALTER TABLE ONLY api_key_models
    ADD CONSTRAINT api_key_models_model_id_fkey FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE CASCADE;
ALTER TABLE ONLY artifact_download_grants
    ADD CONSTRAINT artifact_download_grants_artifact_id_fkey FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE;
ALTER TABLE ONLY artifact_upload_parts
    ADD CONSTRAINT artifact_upload_parts_upload_id_fkey FOREIGN KEY (upload_id) REFERENCES artifact_uploads(id) ON DELETE CASCADE;
ALTER TABLE ONLY artifact_uploads
    ADD CONSTRAINT artifact_uploads_artifact_id_fkey FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE;
ALTER TABLE ONLY debug_trace_manifests
    ADD CONSTRAINT debug_trace_manifests_rejection_id_fkey FOREIGN KEY (rejection_id) REFERENCES rejected_request_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY debug_trace_manifests
    ADD CONSTRAINT debug_trace_manifests_run_id_fkey FOREIGN KEY (run_id) REFERENCES inference_run_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY inference_run_observations
    ADD CONSTRAINT inference_run_observations_interaction_id_fkey FOREIGN KEY (interaction_id) REFERENCES interaction_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY inference_run_observations
    ADD CONSTRAINT inference_run_observations_parent_run_id_fkey FOREIGN KEY (parent_run_id) REFERENCES inference_run_observations(id) ON DELETE SET NULL;
ALTER TABLE ONLY interaction_observations
    ADD CONSTRAINT interaction_observations_parent_interaction_id_fkey FOREIGN KEY (parent_interaction_id) REFERENCES interaction_observations(id) ON DELETE SET NULL;
ALTER TABLE ONLY media_derivatives
    ADD CONSTRAINT media_derivatives_derivative_artifact_id_fkey FOREIGN KEY (derivative_artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE;
ALTER TABLE ONLY media_derivatives
    ADD CONSTRAINT media_derivatives_source_artifact_id_fkey FOREIGN KEY (source_artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE;
ALTER TABLE ONLY model_backends
    ADD CONSTRAINT model_backends_model_id_fkey FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE CASCADE;
ALTER TABLE ONLY model_backends
    ADD CONSTRAINT model_backends_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id);
ALTER TABLE ONLY model_turn_observations
    ADD CONSTRAINT model_turn_observations_interaction_id_fkey FOREIGN KEY (interaction_id) REFERENCES interaction_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY model_turn_observations
    ADD CONSTRAINT model_turn_observations_run_id_fkey FOREIGN KEY (run_id) REFERENCES inference_run_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY native_compaction_sources
    ADD CONSTRAINT native_compaction_sources_record_id_fkey FOREIGN KEY (record_id) REFERENCES native_compactions(id) ON DELETE CASCADE;
ALTER TABLE ONLY native_compaction_sources
    ADD CONSTRAINT native_compaction_sources_source_id_fkey FOREIGN KEY (source_id) REFERENCES native_compactions(id) ON DELETE RESTRICT;
ALTER TABLE ONLY native_compaction_states
    ADD CONSTRAINT native_compaction_states_record_id_fkey FOREIGN KEY (record_id) REFERENCES native_compactions(id) ON DELETE CASCADE;
ALTER TABLE ONLY native_compactions
    ADD CONSTRAINT native_compactions_source_generation_id_fkey FOREIGN KEY (source_generation_id) REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT;
ALTER TABLE ONLY observation_events
    ADD CONSTRAINT observation_events_interaction_id_fkey FOREIGN KEY (interaction_id) REFERENCES interaction_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY observation_events
    ADD CONSTRAINT observation_events_rejection_id_fkey FOREIGN KEY (rejection_id) REFERENCES rejected_request_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY observation_events
    ADD CONSTRAINT observation_events_run_id_fkey FOREIGN KEY (run_id) REFERENCES inference_run_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY observation_pending_tools
    ADD CONSTRAINT observation_pending_tools_interaction_id_fkey FOREIGN KEY (interaction_id) REFERENCES interaction_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY observation_pending_tools
    ADD CONSTRAINT observation_pending_tools_run_id_fkey FOREIGN KEY (run_id) REFERENCES inference_run_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY observation_tail_sources
    ADD CONSTRAINT observation_tail_sources_interaction_id_fkey FOREIGN KEY (interaction_id) REFERENCES interaction_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY observation_tail_sources
    ADD CONSTRAINT observation_tail_sources_run_id_fkey FOREIGN KEY (run_id) REFERENCES inference_run_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY provider_allowance_samples
    ADD CONSTRAINT provider_allowance_samples_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE;
ALTER TABLE ONLY provider_model_cost_rules
    ADD CONSTRAINT provider_model_cost_rules_provider_id_model_id_fkey FOREIGN KEY (provider_id, model_id) REFERENCES provider_models(provider_id, model_id) ON DELETE CASCADE;
ALTER TABLE ONLY provider_models
    ADD CONSTRAINT provider_models_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE;
ALTER TABLE ONLY provider_oauth_credentials
    ADD CONSTRAINT provider_oauth_credentials_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE;
ALTER TABLE ONLY target_attempt_observations
    ADD CONSTRAINT target_attempt_observations_interaction_id_fkey FOREIGN KEY (interaction_id) REFERENCES interaction_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY target_attempt_observations
    ADD CONSTRAINT target_attempt_observations_model_turn_id_fkey FOREIGN KEY (model_turn_id) REFERENCES model_turn_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY target_attempt_observations
    ADD CONSTRAINT target_attempt_observations_run_id_fkey FOREIGN KEY (run_id) REFERENCES inference_run_observations(id) ON DELETE CASCADE;
ALTER TABLE ONLY turn_chain_content_refs
    ADD CONSTRAINT turn_chain_content_refs_node_id_principal_fkey FOREIGN KEY (node_id, principal) REFERENCES turn_chain_nodes(id, principal) ON DELETE CASCADE;
ALTER TABLE ONLY turn_chain_content_refs
    ADD CONSTRAINT turn_chain_content_refs_principal_content_key_fkey FOREIGN KEY (principal, content_key) REFERENCES turn_chain_contents(principal, content_key);
ALTER TABLE ONLY turn_chain_nodes
    ADD CONSTRAINT turn_chain_nodes_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT;
ALTER TABLE ONLY vendor_data_recovery
    ADD CONSTRAINT vendor_data_recovery_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE;
ALTER TABLE ONLY vendor_private_state
    ADD CONSTRAINT vendor_private_state_provider_id_fkey FOREIGN KEY (provider_id) REFERENCES providers(id) ON DELETE CASCADE;

-- Seed rows every fresh database needs: the built-in Local Web Provider with
-- its default selection. Observation ordering uses the real sequence.
INSERT INTO web_providers (id, name, kind, api_key, use_proxy, local_engines)
VALUES (
    'web-provider-local',
    'Local',
    'local',
    NULL,
    FALSE,
    jsonb_build_object(
        'google', jsonb_build_object('enabled', TRUE),
        'bing', jsonb_build_object('enabled', TRUE),
        'brave', jsonb_build_object('enabled', TRUE),
        'baidu', jsonb_build_object('enabled', TRUE),
        '360', jsonb_build_object('enabled', FALSE),
        'sogou_weixin', jsonb_build_object('enabled', FALSE),
        'google_scholar', jsonb_build_object('enabled', FALSE)
    )
);

INSERT INTO settings (name, value)
VALUES
    ('web_access_search_provider_ids', '["web-provider-local"]'),
    ('web_access_fetch_provider_ids', '["web-provider-local"]');
