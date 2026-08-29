CREATE TABLE providers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    vendor            TEXT,
    protocol          TEXT NOT NULL,
    base_url          TEXT NOT NULL,
    preset_key        TEXT,
    channel           TEXT,
    models_source     TEXT,
    static_models     TEXT,
    api_key           TEXT NOT NULL,
    auth_mode         TEXT NOT NULL DEFAULT 'apikey' CHECK (auth_mode IN ('apikey', 'oauth')),
    access_token      TEXT,
    refresh_token     TEXT,
    expires_at        TEXT,
    use_proxy         INTEGER DEFAULT 0,
    last_test_success INTEGER,
    last_test_at      TEXT,
    is_enabled        INTEGER DEFAULT 1,
    priority          INTEGER DEFAULT 0,
    created_at        TEXT DEFAULT (datetime('now')),
    updated_at        TEXT DEFAULT (datetime('now'))
);

CREATE TABLE models (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    balance         TEXT DEFAULT 'weighted',
    target_provider TEXT NOT NULL REFERENCES providers(id),
    target_model    TEXT NOT NULL,
    enable_auth     INTEGER DEFAULT 0,
    enable_payload  INTEGER,
    is_enabled      INTEGER DEFAULT 1,
    priority        INTEGER DEFAULT 0,
    created_at      TEXT DEFAULT (datetime('now'))
);

CREATE TABLE model_backends (
    id          TEXT PRIMARY KEY,
    model_id    TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id),
    model       TEXT NOT NULL,
    weight      INTEGER DEFAULT 100,
    priority    INTEGER DEFAULT 1,
    created_at  TEXT DEFAULT (datetime('now'))
);

CREATE INDEX idx_model_backends_model_id ON model_backends(model_id);

CREATE TABLE api_keys (
    id         TEXT PRIMARY KEY,
    token      TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    rpm        INTEGER,
    rpd        INTEGER,
    tpm        INTEGER,
    tpd        INTEGER,
    is_enabled INTEGER DEFAULT 1,
    expires_at TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now'))
);

CREATE INDEX idx_api_keys_token ON api_keys(token);

CREATE TABLE api_key_models (
    api_key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model_id   TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    PRIMARY KEY (api_key_id, model_id)
);

CREATE INDEX idx_api_key_models_model_id ON api_key_models(model_id);

CREATE TABLE provider_oauth_credentials (
    provider_id     TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    driver_key      TEXT NOT NULL DEFAULT '',
    scheme          TEXT NOT NULL DEFAULT '',
    access_token    TEXT NOT NULL DEFAULT '',
    refresh_token   TEXT,
    expires_at      TEXT,
    resource_url    TEXT,
    subject_id      TEXT,
    scopes          TEXT NOT NULL DEFAULT '[]',
    meta            TEXT NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL DEFAULT 'connected',
    status_version  INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    last_refresh_at TEXT,
    created_at      TEXT DEFAULT (datetime('now')),
    updated_at      TEXT DEFAULT (datetime('now'))
);

CREATE INDEX idx_oauth_creds_status ON provider_oauth_credentials(status);
CREATE INDEX idx_oauth_creds_expires ON provider_oauth_credentials(expires_at);

CREATE TABLE request_logs (
    id                        TEXT PRIMARY KEY,
    created_at                INTEGER NOT NULL DEFAULT 0,
    api_key_id                TEXT,
    api_key_name              TEXT,
    client_protocol           TEXT,
    upstream_protocol         TEXT,
    provider_id               TEXT,
    provider_name             TEXT,
    model_id                  TEXT,
    model_name                TEXT,
    upstream_url              TEXT,
    client_model              TEXT,
    upstream_model            TEXT,
    method                    TEXT,
    path                      TEXT,
    client_request_headers    TEXT,
    client_request_body       TEXT,
    client_response_headers   TEXT,
    client_response_body      TEXT,
    upstream_request_headers  TEXT,
    upstream_request_body     TEXT,
    upstream_response_headers TEXT,
    upstream_response_body    TEXT,
    upstream_status_code      INTEGER,
    client_status_code        INTEGER,
    latency_total_ms          INTEGER,
    latency_upstream_ms       INTEGER,
    input_tokens              INTEGER DEFAULT 0,
    output_tokens             INTEGER DEFAULT 0,
    cache_read_tokens         INTEGER DEFAULT 0,
    is_stream                 INTEGER DEFAULT 0,
    stream_chunks_count       INTEGER DEFAULT 0,
    stream_first_chunk_ms     INTEGER
);

CREATE INDEX idx_logs_created_at ON request_logs(created_at);
CREATE INDEX idx_logs_provider_id ON request_logs(provider_id);
CREATE INDEX idx_logs_client_status ON request_logs(client_status_code);
CREATE INDEX idx_logs_upstream_model ON request_logs(upstream_model);
CREATE INDEX idx_logs_api_key ON request_logs(api_key_id);
CREATE INDEX idx_logs_client_protocol ON request_logs(client_protocol);
CREATE INDEX idx_logs_upstream_protocol ON request_logs(upstream_protocol);

CREATE TABLE settings (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT DEFAULT (datetime('now'))
);
