DROP TABLE IF EXISTS request_logs;

CREATE TABLE observation_sequence (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    next_sequence INTEGER NOT NULL
);
INSERT INTO observation_sequence (singleton_id, next_sequence) VALUES (1, 1);

CREATE TABLE interaction_observations (
    id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    api_key_id TEXT,
    api_key_name TEXT,
    generation_root_id TEXT,
    parent_interaction_id TEXT REFERENCES interaction_observations(id) ON DELETE SET NULL,
    root_id TEXT NOT NULL,
    root_run_id TEXT NOT NULL,
    first_route_id TEXT NOT NULL,
    first_model_display_name TEXT,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    last_active_at INTEGER NOT NULL,
    visible_tail TEXT NOT NULL DEFAULT '',
    input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
    cache_write_tokens INTEGER, reasoning_tokens INTEGER,
    observation_gap INTEGER NOT NULL DEFAULT 0,
    last_event_sequence INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER NOT NULL
);
CREATE INDEX interaction_observations_window_idx ON interaction_observations(root_id, last_active_at DESC, id);
CREATE INDEX interaction_observations_generation_idx ON interaction_observations(generation_root_id, last_active_at);
CREATE INDEX interaction_observations_filter_idx ON interaction_observations(status, api_key_name, last_active_at DESC);
CREATE INDEX interaction_observations_expiry_idx ON interaction_observations(expires_at);

CREATE TABLE inference_run_observations (
    id TEXT PRIMARY KEY,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    parent_run_id TEXT REFERENCES inference_run_observations(id) ON DELETE SET NULL,
    generation_node_id TEXT,
    generation_parent_id TEXT,
    ingress_protocol TEXT NOT NULL,
    route_id TEXT NOT NULL,
    model_display_name TEXT,
    status TEXT NOT NULL,
    terminal_reason TEXT,
    user_interrupted INTEGER NOT NULL DEFAULT 0,
    background_active INTEGER NOT NULL DEFAULT 0,
    debug_enabled INTEGER NOT NULL,
    client_output_committed INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER NOT NULL,
    last_active_at INTEGER NOT NULL,
    finished_at INTEGER,
    last_event_sequence INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER NOT NULL
);
CREATE INDEX inference_runs_interaction_idx ON inference_run_observations(interaction_id, started_at, id);
CREATE INDEX inference_runs_generation_idx ON inference_run_observations(generation_node_id, generation_parent_id);
CREATE INDEX inference_runs_status_idx ON inference_run_observations(status, last_active_at);

CREATE TABLE model_turn_observations (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    route_id TEXT NOT NULL,
    model_display_name TEXT,
    api_key_id TEXT,
    api_key_name TEXT,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
    cache_write_tokens INTEGER, reasoning_tokens INTEGER,
    last_event_sequence INTEGER NOT NULL
);
CREATE INDEX model_turns_interaction_idx ON model_turn_observations(interaction_id, started_at, id);
CREATE INDEX model_turns_analytics_idx ON model_turn_observations(started_at, route_id, api_key_id, status);

CREATE TABLE target_attempt_observations (
    id TEXT PRIMARY KEY,
    model_turn_id TEXT NOT NULL REFERENCES model_turn_observations(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL, provider_id TEXT NOT NULL, provider_name TEXT NOT NULL,
    upstream_model TEXT NOT NULL, protocol TEXT NOT NULL,
    status TEXT NOT NULL, status_code INTEGER, error_code TEXT,
    started_at INTEGER NOT NULL, finished_at INTEGER, duration_ms INTEGER, first_token_ms INTEGER,
    input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
    cache_write_tokens INTEGER, reasoning_tokens INTEGER,
    usage_recorded INTEGER NOT NULL DEFAULT 0,
    last_event_sequence INTEGER NOT NULL
);
CREATE INDEX target_attempts_turn_idx ON target_attempt_observations(model_turn_id, started_at, id);
CREATE INDEX target_attempts_analytics_idx ON target_attempt_observations(started_at, provider_id, upstream_model, target_id, status);

CREATE TABLE rejected_request_observations (
    id TEXT PRIMARY KEY, occurred_at INTEGER NOT NULL, method TEXT NOT NULL, path TEXT NOT NULL,
    ingress_protocol TEXT NOT NULL, stage TEXT NOT NULL, code TEXT NOT NULL, status_code INTEGER NOT NULL,
    debug_enabled INTEGER NOT NULL, debug_status TEXT NOT NULL, last_event_sequence INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX rejected_requests_window_idx ON rejected_request_observations(occurred_at DESC, id);
CREATE INDEX rejected_requests_expiry_idx ON rejected_request_observations(expires_at);

CREATE TABLE debug_trace_manifests (
    trace_id TEXT PRIMARY KEY,
    run_id TEXT REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    rejection_id TEXT REFERENCES rejected_request_observations(id) ON DELETE CASCADE,
    relative_directory TEXT NOT NULL UNIQUE,
    bytes_written INTEGER NOT NULL DEFAULT 0, event_count INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL, partial_reason TEXT, tombstoned INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL, completed_at INTEGER, expires_at INTEGER NOT NULL,
    CHECK ((run_id IS NOT NULL) <> (rejection_id IS NOT NULL))
);
CREATE INDEX debug_manifests_expiry_idx ON debug_trace_manifests(tombstoned, expires_at);

CREATE TABLE observation_events (
    sequence INTEGER PRIMARY KEY,
    occurred_at INTEGER NOT NULL,
    interaction_id TEXT REFERENCES interaction_observations(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    rejection_id TEXT REFERENCES rejected_request_observations(id) ON DELETE CASCADE,
    kind TEXT NOT NULL, payload TEXT NOT NULL, expires_at INTEGER NOT NULL
);
CREATE INDEX observation_events_interaction_idx ON observation_events(interaction_id, sequence);
CREATE INDEX observation_events_run_idx ON observation_events(run_id, sequence);
CREATE INDEX observation_events_rejection_idx ON observation_events(rejection_id, sequence);
CREATE INDEX observation_events_expiry_idx ON observation_events(expires_at, sequence);
