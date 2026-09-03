-- Stravia AI Gateway - PostgreSQL Final Schema
--
-- This file represents the authoritative final-state schema after all migrations.
-- It is a DBA review artifact only. Do not execute it to initialize a Stravia
-- database: direct execution does not record SQLx migration history.
-- Start stravia-server with a blank database so it can apply the migrations.
--
-- Generated from: backend/crates/stravia-core/migrations/postgres/
-- Regenerate  : stravia-tools dump-schema --backend postgres
--
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
    expires_at        TIMESTAMPTZ,
    use_proxy         BOOLEAN NOT NULL DEFAULT FALSE,
    last_test_success BOOLEAN,
    last_test_at      TIMESTAMPTZ,
    is_enabled        BOOLEAN DEFAULT TRUE,
    priority          INTEGER DEFAULT 0,
    created_at        TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at        TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE models (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    balance         TEXT DEFAULT 'weighted',
    target_provider TEXT NOT NULL REFERENCES providers(id),
    target_model    TEXT NOT NULL,
    enable_auth     BOOLEAN DEFAULT FALSE,
    enable_payload  BOOLEAN,
    is_enabled      BOOLEAN DEFAULT TRUE,
    priority        INTEGER DEFAULT 0,
    created_at      TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE model_backends (
    id          TEXT PRIMARY KEY,
    model_id    TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers(id),
    model       TEXT NOT NULL,
    weight      INTEGER DEFAULT 100,
    priority    INTEGER DEFAULT 1,
    created_at  TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
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
    is_enabled BOOLEAN DEFAULT TRUE,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
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
    expires_at      TIMESTAMPTZ,
    resource_url    TEXT,
    subject_id      TEXT,
    scopes          TEXT NOT NULL DEFAULT '[]',
    meta            TEXT NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL DEFAULT 'connected',
    status_version  INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    last_refresh_at TIMESTAMPTZ,
    created_at      TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_oauth_creds_status ON provider_oauth_credentials(status);
CREATE INDEX idx_oauth_creds_expires ON provider_oauth_credentials(expires_at);

CREATE TABLE request_logs (
    id                        TEXT PRIMARY KEY,
    created_at                BIGINT NOT NULL DEFAULT 0,
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
    latency_total_ms          BIGINT,
    latency_upstream_ms       BIGINT,
    input_tokens              INTEGER DEFAULT 0,
    output_tokens             INTEGER DEFAULT 0,
    cache_read_tokens         INTEGER DEFAULT 0,
    is_stream                 BOOLEAN DEFAULT FALSE,
    stream_chunks_count       INTEGER DEFAULT 0,
    stream_first_chunk_ms     BIGINT
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
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

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

ALTER TABLE api_keys
ADD COLUMN web_access_enabled BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE web_providers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL CHECK (kind IN ('codex', 'exa', 'brave', 'tavily')),
    api_key           TEXT,
    provider_id       TEXT REFERENCES providers(id) ON DELETE CASCADE,
    last_test_success BOOLEAN,
    last_test_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (
        (kind = 'codex' AND provider_id IS NOT NULL AND api_key IS NULL)
        OR (kind IN ('exa', 'brave', 'tavily') AND api_key IS NOT NULL AND provider_id IS NULL)
    )
);

CREATE INDEX idx_web_providers_provider_id ON web_providers(provider_id);

ALTER TABLE provider_oauth_credentials
    ADD COLUMN connection_id TEXT;

UPDATE provider_oauth_credentials
SET connection_id = 'legacy-' || provider_id
WHERE connection_id IS NULL;

ALTER TABLE provider_oauth_credentials
    ALTER COLUMN connection_id SET NOT NULL;

CREATE UNIQUE INDEX idx_oauth_creds_connection_id
    ON provider_oauth_credentials(connection_id);

ALTER TABLE api_keys
RENAME COLUMN web_access_enabled TO mcp_access_enabled;

ALTER TABLE api_keys
ADD COLUMN web_search_injection_enabled BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE api_keys
SET web_search_injection_enabled = mcp_access_enabled;

ALTER TABLE web_providers
DROP CONSTRAINT web_providers_kind_check,
DROP CONSTRAINT web_providers_check;

ALTER TABLE web_providers
ADD CONSTRAINT web_providers_kind_check
    CHECK (kind IN ('codex', 'exa', 'brave', 'tavily', 'zhipu')),
ADD CONSTRAINT web_providers_credentials_check
    CHECK (
        (kind = 'codex' AND provider_id IS NOT NULL AND api_key IS NULL)
        OR (kind IN ('exa', 'brave', 'tavily', 'zhipu') AND api_key IS NOT NULL AND provider_id IS NULL)
    );

CREATE TABLE turn_chain_nodes (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('response', 'agent')),
    parent_id       TEXT REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT,
    principal       TEXT NOT NULL,
    payload_version BIGINT NOT NULL CHECK (payload_version > 0),
    payload         TEXT NOT NULL,
    created_at      BIGINT NOT NULL,
    expires_at      BIGINT NOT NULL
);

CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes(principal, kind);
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes(expires_at);

CREATE TABLE agent_definition_revisions (
    definition_id TEXT NOT NULL,
    slug          TEXT NOT NULL,
    version       BIGINT NOT NULL CHECK (version > 0),
    spec_hash     TEXT NOT NULL,
    spec_json     TEXT NOT NULL,
    created_at    BIGINT NOT NULL,
    PRIMARY KEY (definition_id, version),
    UNIQUE (slug, version)
);

CREATE TABLE agent_definition_configs (
    definition_id TEXT PRIMARY KEY,
    enabled       BOOLEAN NOT NULL DEFAULT FALSE,
    model_id      TEXT REFERENCES models(id) ON DELETE SET NULL,
    updated_at    BIGINT NOT NULL
);

CREATE INDEX idx_agent_definition_revisions_slug
    ON agent_definition_revisions(slug, version);

CREATE TABLE artifacts (
    id           TEXT PRIMARY KEY,
    principal    TEXT NOT NULL,
    mime_type    TEXT NOT NULL,
    size         BIGINT NOT NULL CHECK (size >= 0),
    backend_key  TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('staging', 'ready')),
    expires_at   BIGINT NOT NULL,
    created_at   BIGINT NOT NULL
);

CREATE TABLE artifact_uploads (
    id            TEXT PRIMARY KEY,
    artifact_id   TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    principal     TEXT NOT NULL,
    token_hash    TEXT NOT NULL,
    declared_size BIGINT NOT NULL CHECK (declared_size >= 0),
    received_size BIGINT NOT NULL DEFAULT 0 CHECK (received_size >= 0),
    expires_at    BIGINT NOT NULL,
    created_at    BIGINT NOT NULL
);

CREATE TABLE artifact_upload_parts (
    upload_id   TEXT NOT NULL REFERENCES artifact_uploads(id) ON DELETE CASCADE,
    part_number BIGINT NOT NULL CHECK (part_number > 0),
    etag        TEXT NOT NULL,
    size        BIGINT NOT NULL CHECK (size >= 0),
    PRIMARY KEY (upload_id, part_number)
);

CREATE INDEX idx_artifacts_expiry ON artifacts(expires_at);
CREATE INDEX idx_artifact_uploads_expiry ON artifact_uploads(expires_at);

ALTER TABLE api_keys
ADD COLUMN allow_web_research BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE api_keys
SET allow_web_research = TRUE
WHERE mcp_access_enabled OR web_search_injection_enabled;

INSERT INTO settings (name, value)
SELECT
    'web_research_config',
    jsonb_build_object(
        'revision', 0,
        'enabled', FALSE,
        'backend', jsonb_build_object(
            'kind', 'codex',
            'provider_id', provider_id,
            'upstream_model', NULL
        ),
        'max_turns', 12,
        'total_time_seconds', 600,
        'updated_at', ''
    )::text
FROM web_providers
WHERE kind = 'codex'
  AND (SELECT COUNT(*) FROM web_providers WHERE kind = 'codex') = 1
ON CONFLICT (name) DO NOTHING;

UPDATE settings AS s
SET value = (
    SELECT COALESCE(
        jsonb_agg(entry.value ORDER BY entry.ordinality),
        '[]'::jsonb
    )::text
    FROM jsonb_array_elements_text(s.value::jsonb)
        WITH ORDINALITY AS entry(value, ordinality)
    WHERE entry.value NOT IN (
        SELECT id FROM web_providers WHERE kind = 'codex'
    )
),
updated_at = CURRENT_TIMESTAMP
WHERE s.name IN (
    'web_access_search_provider_ids',
    'web_access_fetch_provider_ids'
);

DELETE FROM web_providers WHERE kind = 'codex';

ALTER TABLE web_providers
DROP CONSTRAINT web_providers_kind_check,
DROP CONSTRAINT web_providers_credentials_check;

ALTER TABLE web_providers
DROP COLUMN provider_id,
ALTER COLUMN api_key SET NOT NULL,
ADD CONSTRAINT web_providers_kind_check
    CHECK (kind IN ('exa', 'brave', 'tavily', 'zhipu'));

ALTER TABLE turn_chain_nodes
DROP CONSTRAINT turn_chain_nodes_kind_check,
ADD CONSTRAINT turn_chain_nodes_kind_check
    CHECK (kind IN ('response', 'agent', 'web_research'));

ALTER TABLE api_keys
ADD COLUMN allow_media_understanding BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE media_derivatives (
    principal              TEXT NOT NULL,
    source_artifact_id     TEXT PRIMARY KEY REFERENCES artifacts(id) ON DELETE CASCADE,
    derivative_artifact_id TEXT NOT NULL UNIQUE REFERENCES artifacts(id) ON DELETE CASCADE,
    created_at             BIGINT NOT NULL,
    CHECK (source_artifact_id <> derivative_artifact_id)
);

ALTER TABLE models
ADD COLUMN operation TEXT NOT NULL DEFAULT 'inference'
CHECK (operation IN ('inference', 'image_generation'));

ALTER TABLE api_keys
ADD COLUMN image_rpm INTEGER;

ALTER TABLE api_keys
ADD COLUMN image_rpd INTEGER;

ALTER TABLE api_keys
ADD COLUMN allow_image_generation BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE artifacts
ADD COLUMN insecure_transport BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TABLE image_continuations (
    id                 TEXT PRIMARY KEY,
    surface            TEXT NOT NULL CHECK (surface IN ('responses', 'mcp')),
    parent_id          TEXT REFERENCES image_continuations(id),
    principal          TEXT NOT NULL,
    route_id           TEXT NOT NULL,
    target_id          TEXT NOT NULL,
    provider_id        TEXT NOT NULL,
    upstream_model     TEXT NOT NULL,
    endpoint           TEXT NOT NULL,
    continuation_mode  TEXT NOT NULL CHECK (continuation_mode IN ('native', 'artifact_replay')),
    opaque_upstream_id TEXT,
    final_artifacts    JSONB NOT NULL,
    created_at         BIGINT NOT NULL,
    expires_at         BIGINT NOT NULL
);

CREATE INDEX idx_image_continuations_principal_expiry
ON image_continuations(principal, expires_at);
CREATE INDEX idx_image_continuations_parent
ON image_continuations(parent_id);

CREATE TABLE artifact_delivery_tokens (
    token_hash  TEXT PRIMARY KEY,
    principal   TEXT NOT NULL,
    artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    expires_at  BIGINT NOT NULL,
    created_at  BIGINT NOT NULL
);

CREATE INDEX idx_artifact_delivery_tokens_expiry
ON artifact_delivery_tokens(expires_at);

CREATE TABLE image_generation_runs (
    id                    TEXT PRIMARY KEY,
    principal             TEXT NOT NULL,
    operation             TEXT NOT NULL DEFAULT 'image_generation',
    route_id              TEXT,
    target_id             TEXT,
    provider_id           TEXT,
    upstream_model        TEXT,
    requested_images      INTEGER NOT NULL,
    generated_images      INTEGER NOT NULL DEFAULT 0,
    failed_images         INTEGER NOT NULL DEFAULT 0,
    outcome               TEXT NOT NULL,
    usage                 JSONB NOT NULL DEFAULT '{}'::jsonb,
    parent_request_id     TEXT,
    capability_drift      JSONB,
    possible_duplicate_charge BOOLEAN NOT NULL DEFAULT FALSE,
    created_at            BIGINT NOT NULL,
    latency_ms            BIGINT,
    completed_at          BIGINT
);

CREATE INDEX idx_image_generation_runs_principal_created
ON image_generation_runs(principal, created_at);

CREATE TABLE image_generation_attempts (
    id                        TEXT PRIMARY KEY,
    run_id                    TEXT NOT NULL REFERENCES image_generation_runs(id) ON DELETE CASCADE,
    ordinal                   INTEGER NOT NULL,
    target_id                 TEXT NOT NULL,
    provider_id               TEXT NOT NULL,
    upstream_model            TEXT NOT NULL,
    outcome                   TEXT NOT NULL,
    error_code                TEXT,
    possible_duplicate_charge BOOLEAN NOT NULL DEFAULT FALSE,
    provider_request_id       TEXT,
    usage                     JSONB,
    latency_ms                BIGINT,
    created_at                BIGINT NOT NULL,
    completed_at              BIGINT,
    UNIQUE(run_id, ordinal)
);

CREATE INDEX idx_image_generation_attempts_run
ON image_generation_attempts(run_id, ordinal);

CREATE TABLE image_capability_drifts (
    id                TEXT PRIMARY KEY,
    provider_id       TEXT NOT NULL,
    upstream_model    TEXT NOT NULL,
    fingerprint       TEXT NOT NULL,
    safe_message      TEXT NOT NULL,
    suppressed_until  BIGINT NOT NULL,
    created_at        BIGINT NOT NULL,
    UNIQUE(provider_id, upstream_model, fingerprint)
);

ALTER TABLE models
DROP COLUMN enable_auth;

ALTER TABLE turn_chain_nodes ADD COLUMN prefix_namespace TEXT;
ALTER TABLE turn_chain_nodes ADD COLUMN prefix_fingerprint TEXT;
ALTER TABLE turn_chain_nodes ADD COLUMN prefix_item_count BIGINT;
ALTER TABLE turn_chain_nodes ADD COLUMN prefix_completed_at BIGINT;

CREATE INDEX idx_turn_chain_reusable_prefix
ON turn_chain_nodes (
    principal,
    kind,
    prefix_namespace,
    prefix_fingerprint,
    prefix_item_count DESC,
    prefix_completed_at DESC,
    expires_at,
    id DESC
)
WHERE prefix_namespace IS NOT NULL;

DELETE FROM artifact_delivery_tokens WHERE principal = 'anonymous';
DELETE FROM media_derivatives WHERE principal = 'anonymous';
DELETE FROM artifact_uploads WHERE principal = 'anonymous';
DELETE FROM image_generation_runs WHERE principal = 'anonymous';
UPDATE image_continuations SET parent_id = NULL WHERE principal = 'anonymous';
DELETE FROM image_continuations WHERE principal = 'anonymous';
UPDATE turn_chain_nodes SET parent_id = NULL WHERE principal = 'anonymous';
DELETE FROM turn_chain_nodes WHERE principal = 'anonymous';
DELETE FROM artifacts WHERE principal = 'anonymous';

DELETE FROM models WHERE operation = 'image_generation';
DELETE FROM settings WHERE name = 'default_image_route_id';

DROP TABLE image_generation_attempts;
DROP TABLE image_generation_runs;
DROP TABLE image_continuations;
DROP TABLE image_capability_drifts;
DROP TABLE artifact_delivery_tokens;

ALTER TABLE models DROP COLUMN operation;
ALTER TABLE api_keys DROP COLUMN image_rpm;
ALTER TABLE api_keys DROP COLUMN image_rpd;
ALTER TABLE api_keys DROP COLUMN allow_image_generation;
ALTER TABLE artifacts DROP COLUMN insecure_transport;

ALTER TABLE api_keys DROP COLUMN rpm;
ALTER TABLE api_keys DROP COLUMN rpd;
ALTER TABLE api_keys DROP COLUMN tpm;
ALTER TABLE api_keys DROP COLUMN tpd;
ALTER TABLE api_keys
ADD COLUMN concurrency_limit INTEGER CHECK (concurrency_limit > 0);

ALTER TABLE api_keys
ADD COLUMN transparent_injection_enabled BOOLEAN NOT NULL DEFAULT FALSE,
ADD COLUMN inject_media_understanding BOOLEAN NOT NULL DEFAULT FALSE,
ADD COLUMN inject_web_search BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE api_keys
SET transparent_injection_enabled =
        web_search_injection_enabled OR allow_media_understanding,
    inject_media_understanding = allow_media_understanding,
    inject_web_search = web_search_injection_enabled;

ALTER TABLE api_keys
DROP COLUMN web_search_injection_enabled,
DROP COLUMN allow_web_research,
DROP COLUMN allow_media_understanding;

UPDATE settings
SET name = 'web_search_config',
    updated_at = CURRENT_TIMESTAMP
WHERE name = 'web_research_config';

DELETE FROM turn_chain_nodes
WHERE kind = 'web_research';

ALTER TABLE turn_chain_nodes
DROP CONSTRAINT turn_chain_nodes_kind_check,
ADD CONSTRAINT turn_chain_nodes_kind_check
    CHECK (kind IN ('response', 'agent', 'web_search'));

DELETE FROM settings WHERE name = 'enable_payload';

ALTER TABLE models DROP COLUMN enable_payload;

-- Convert only known preset identities. Unidentified legacy values remain
-- visible for administrator repair rather than being mapped to an unrelated
-- Provider Catalog scope.
UPDATE providers
SET
    models_source = 'catalog',
    preset_key = CASE
        WHEN preset_key IS NULL OR btrim(preset_key) = ''
            THEN substring(models_source FROM length('ai://models.dev/') + 1)
        ELSE preset_key
    END
WHERE models_source LIKE 'ai://models.dev/%'
  AND substring(models_source FROM length('ai://models.dev/') + 1) IN (
      'openai', 'anthropic', 'google', 'vertexai', 'xai', 'deepseek',
      'moonshotai', 'minimax', 'zhipuai', 'zai', 'nvidia', 'openrouter', 'ollama'
  )
  AND (
      preset_key IS NULL
      OR btrim(preset_key) = ''
      OR preset_key = substring(models_source FROM length('ai://models.dev/') + 1)
  );

UPDATE providers
SET
    models_source = 'catalog',
    preset_key = CASE
        WHEN preset_key IS NOT NULL AND btrim(preset_key) <> '' THEN preset_key
        ELSE vendor
    END
WHERE models_source = 'ai://models.dev'
  AND (
      preset_key IN (
          'openai', 'anthropic', 'google', 'vertexai', 'xai', 'deepseek',
          'moonshotai', 'minimax', 'zhipuai', 'zai', 'nvidia', 'openrouter', 'ollama'
      )
      OR (
          (preset_key IS NULL OR btrim(preset_key) = '')
          AND vendor IN (
              'openai', 'anthropic', 'google', 'vertexai', 'xai', 'deepseek',
              'moonshotai', 'minimax', 'zhipuai', 'zai', 'nvidia', 'openrouter', 'ollama'
          )
      )
  );
-- Convert only known preset identities. Unidentified legacy values remain

-- Store Vendor-declared credentials separately from the legacy API-key mirror.
ALTER TABLE providers ADD COLUMN adapter_credentials TEXT NOT NULL DEFAULT '{}';

-- Catalog identities are discovery keys. Runtime Vendor identities are keyed by npm package.
UPDATE providers
SET vendor = CASE
        WHEN vendor IN (
            'aihubmix'
        ) THEN 'aihubmix'
        WHEN vendor IN (
            'amazon-bedrock'
        ) THEN 'amazon-bedrock'
        WHEN vendor IN (
            'anthropic',
            'freemodel',
            'kimi-for-coding',
            'minimax',
            'minimax-cn',
            'minimax-cn-coding-plan',
            'minimax-coding-plan',
            'subconscious',
            'thinkingmachines'
        ) THEN 'anthropic'
        WHEN vendor IN (
            'azure',
            'azure-cognitive-services'
        ) THEN 'azure'
        WHEN vendor IN (
            'cerebras'
        ) THEN 'cerebras'
        WHEN vendor IN (
            'cloudflare-ai-gateway'
        ) THEN 'cloudflare-ai-gateway'
        WHEN vendor IN (
            'cohere'
        ) THEN 'cohere'
        WHEN vendor IN (
            'deepinfra'
        ) THEN 'deepinfra'
        WHEN vendor IN (
            'vercel'
        ) THEN 'gateway'
        WHEN vendor IN (
            'gitlab'
        ) THEN 'gitlab'
        WHEN vendor IN (
            'google'
        ) THEN 'google'
        WHEN vendor IN (
            'google-vertex'
        ) THEN 'google-vertex'
        WHEN vendor IN (
            'google-vertex-anthropic'
        ) THEN 'google-vertex-anthropic'
        WHEN vendor IN (
            'groq'
        ) THEN 'groq'
        WHEN vendor IN (
            'merge-gateway'
        ) THEN 'merge-gateway'
        WHEN vendor IN (
            'mistral'
        ) THEN 'mistral'
        WHEN vendor IN (
            'meta',
            'openai',
            'perplexity-agent',
            'vivgrid'
        ) THEN 'openai'
        WHEN vendor IN (
            '302ai',
            'abacus',
            'abliteration-ai',
            'ai-router',
            'aiand',
            'aki-io',
            'alibaba',
            'alibaba-cn',
            'alibaba-coding-plan',
            'alibaba-coding-plan-cn',
            'alibaba-token-plan',
            'alibaba-token-plan-cn',
            'ambient',
            'amd',
            'anyapi',
            'arcee',
            'atomic-chat',
            'auriko',
            'bailing',
            'baseten',
            'berget',
            'blueclaw',
            'chutes',
            'clarifai',
            'claudinio',
            'cline-pass',
            'cloudferro-sherlock',
            'cloudflare-workers-ai',
            'coralbricks',
            'cortecs',
            'crof',
            'crossmodel',
            'crusoe',
            'daoxe',
            'databricks',
            'deepseek',
            'digitalocean',
            'dinference',
            'drun',
            'ebcloud',
            'echo',
            'edenai',
            'empiriolabs',
            'evroc',
            'fastrouter',
            'fireworks-ai',
            'friendli',
            'frogbot',
            'github-copilot',
            'gmicloud',
            'greenpt',
            'helicone',
            'hetzner',
            'hpc-ai',
            'huggingface',
            'hyper',
            'iflowcn',
            'impossibl',
            'inception',
            'inceptron',
            'inference',
            'inferx',
            'infomaniak',
            'io-net',
            'jalapeno',
            'jiekou',
            'kenari',
            'kilo',
            'kosmik',
            'kuae-cloud-coding-plan',
            'lilac',
            'llama',
            'llmgateway',
            'llmtr',
            'lmstudio',
            'longcat',
            'lucidquery',
            'lynkr',
            'meganova',
            'mixlayer',
            'moark',
            'modal',
            'model-oracle-ai',
            'modelis',
            'modelscope',
            'moonshotai',
            'moonshotai-cn',
            'morph',
            'nano-gpt',
            'nearai',
            'nebius',
            'neon',
            'neuralwatt',
            'nova',
            'novita-ai',
            'nvidia',
            'ofox',
            'ollama-cloud',
            'opencode',
            'opencode-go',
            'orcarouter',
            'ovhcloud',
            'pioneer',
            'poe',
            'poolside',
            'privatemode-ai',
            'qihang-ai',
            'qiniu-ai',
            'regolo-ai',
            'requesty',
            'routing-run',
            'runinfra',
            'sakana',
            'sarvam',
            'scaleway',
            'scnet-token-plan',
            'scx-ai',
            'siliconflow',
            'siliconflow-cn',
            'snowflake-cortex',
            'stackit',
            'stepfun',
            'stepfun-ai',
            'stepfun-ai-step-plan',
            'stepfun-step-plan',
            'submodel',
            'synthetic',
            'tencent-coding-plan',
            'tencent-token-plan',
            'tencent-tokenhub',
            'tensorx',
            'the-grid-ai',
            'tinfoil',
            'trustedrouter',
            'umans-ai',
            'umans-ai-coding-plan',
            'unorouter',
            'upstage',
            'vultr',
            'wafer.ai',
            'wandb',
            'xiaomi',
            'xiaomi-token-plan-ams',
            'xiaomi-token-plan-cn',
            'xiaomi-token-plan-sgp',
            'xpersona',
            'zai',
            'zai-coding-plan',
            'zeldoc',
            'zenifra',
            'zenmux',
            'zhipuai',
            'zhipuai-coding-plan'
        ) THEN 'openai-compatible'
        WHEN vendor IN (
            'openrouter'
        ) THEN 'openrouter'
        WHEN vendor IN (
            'perplexity'
        ) THEN 'perplexity'
        WHEN vendor IN (
            'qvac'
        ) THEN 'qvac'
        WHEN vendor IN (
            'salad-cloud'
        ) THEN 'salad-cloud'
        WHEN vendor IN (
            'sap-ai-core'
        ) THEN 'sap-ai-core'
        WHEN vendor IN (
            'togetherai'
        ) THEN 'togetherai'
        WHEN vendor IN (
            'venice'
        ) THEN 'venice'
        WHEN vendor IN (
            'v0'
        ) THEN 'vercel'
        WHEN vendor IN (
            'watsonx'
        ) THEN 'watsonx'
        WHEN vendor IN (
            'xai'
        ) THEN 'xai'
        WHEN vendor = 'vertexai' THEN 'google-vertex'
        ELSE vendor
    END
WHERE vendor IN (
        '302ai',
        'abacus',
        'abliteration-ai',
        'ai-router',
        'aiand',
        'aihubmix',
        'aki-io',
        'alibaba',
        'alibaba-cn',
        'alibaba-coding-plan',
        'alibaba-coding-plan-cn',
        'alibaba-token-plan',
        'alibaba-token-plan-cn',
        'amazon-bedrock',
        'ambient',
        'amd',
        'anthropic',
        'anyapi',
        'arcee',
        'atomic-chat',
        'auriko',
        'azure',
        'azure-cognitive-services',
        'bailing',
        'baseten',
        'berget',
        'blueclaw',
        'cerebras',
        'chutes',
        'clarifai',
        'claudinio',
        'cline-pass',
        'cloudferro-sherlock',
        'cloudflare-ai-gateway',
        'cloudflare-workers-ai',
        'cohere',
        'coralbricks',
        'cortecs',
        'crof',
        'crossmodel',
        'crusoe',
        'daoxe',
        'databricks',
        'deepinfra',
        'deepseek',
        'digitalocean',
        'dinference',
        'drun',
        'ebcloud',
        'echo',
        'edenai',
        'empiriolabs',
        'evroc',
        'fastrouter',
        'fireworks-ai',
        'freemodel',
        'friendli',
        'frogbot',
        'github-copilot',
        'gitlab',
        'gmicloud',
        'google',
        'google-vertex',
        'google-vertex-anthropic',
        'greenpt',
        'groq',
        'helicone',
        'hetzner',
        'hpc-ai',
        'huggingface',
        'hyper',
        'iflowcn',
        'impossibl',
        'inception',
        'inceptron',
        'inference',
        'inferx',
        'infomaniak',
        'io-net',
        'jalapeno',
        'jiekou',
        'kenari',
        'kilo',
        'kimi-for-coding',
        'kosmik',
        'kuae-cloud-coding-plan',
        'lilac',
        'llama',
        'llmgateway',
        'llmtr',
        'lmstudio',
        'longcat',
        'lucidquery',
        'lynkr',
        'meganova',
        'merge-gateway',
        'meta',
        'minimax',
        'minimax-cn',
        'minimax-cn-coding-plan',
        'minimax-coding-plan',
        'mistral',
        'mixlayer',
        'moark',
        'modal',
        'model-oracle-ai',
        'modelis',
        'modelscope',
        'moonshotai',
        'moonshotai-cn',
        'morph',
        'nano-gpt',
        'nearai',
        'nebius',
        'neon',
        'neuralwatt',
        'nova',
        'novita-ai',
        'nvidia',
        'ofox',
        'ollama-cloud',
        'openai',
        'opencode',
        'opencode-go',
        'openrouter',
        'orcarouter',
        'ovhcloud',
        'perplexity',
        'perplexity-agent',
        'pioneer',
        'poe',
        'poolside',
        'privatemode-ai',
        'qihang-ai',
        'qiniu-ai',
        'qvac',
        'regolo-ai',
        'requesty',
        'routing-run',
        'runinfra',
        'sakana',
        'salad-cloud',
        'sap-ai-core',
        'sarvam',
        'scaleway',
        'scnet-token-plan',
        'scx-ai',
        'siliconflow',
        'siliconflow-cn',
        'snowflake-cortex',
        'stackit',
        'stepfun',
        'stepfun-ai',
        'stepfun-ai-step-plan',
        'stepfun-step-plan',
        'subconscious',
        'submodel',
        'synthetic',
        'tencent-coding-plan',
        'tencent-token-plan',
        'tencent-tokenhub',
        'tensorx',
        'the-grid-ai',
        'thinkingmachines',
        'tinfoil',
        'togetherai',
        'trustedrouter',
        'umans-ai',
        'umans-ai-coding-plan',
        'unorouter',
        'upstage',
        'v0',
        'venice',
        'vercel',
        'vivgrid',
        'vultr',
        'wafer.ai',
        'wandb',
        'watsonx',
        'xai',
        'xiaomi',
        'xiaomi-token-plan-ams',
        'xiaomi-token-plan-cn',
        'xiaomi-token-plan-sgp',
        'xpersona',
        'zai',
        'zai-coding-plan',
        'zeldoc',
        'zenifra',
        'zenmux',
        'zhipuai',
        'zhipuai-coding-plan'
    )
    OR vendor = 'vertexai';

-- Preserve every existing secret under the Vendor field it now represents.
UPDATE providers
SET adapter_credentials = CASE
    WHEN (vendor IN ('google-vertex', 'google-vertex-anthropic')
          OR preset_key IN ('vertexai', 'google-vertex', 'google-vertex-anthropic'))
         AND ltrim(api_key) LIKE '{%' THEN jsonb_build_object('credentials', api_key)::text
    WHEN btrim(api_key) <> '' THEN jsonb_build_object('apiKey', api_key)::text
    ELSE '{}'
END;

ALTER TABLE models
ADD COLUMN supported_thinking_levels JSONB NOT NULL DEFAULT '[]'::jsonb;

ALTER TABLE model_backends
ADD COLUMN thinking_level_map JSONB NOT NULL DEFAULT '[{"level":"off","control":{"type":"hidden"},"source":"generated"},{"level":"minimal","control":{"type":"hidden"},"source":"generated"},{"level":"low","control":{"type":"hidden"},"source":"generated"},{"level":"medium","control":{"type":"hidden"},"source":"generated"},{"level":"high","control":{"type":"hidden"},"source":"generated"},{"level":"xhigh","control":{"type":"hidden"},"source":"generated"},{"level":"max","control":{"type":"hidden"},"source":"generated"}]'::jsonb;

CREATE TABLE history_markers (
    reference TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('platform', 'thinking')),
    activity TEXT NOT NULL,
    tool_id TEXT,
    call_payload TEXT,
    segment_payload TEXT,
    execution_state TEXT CHECK (
        execution_state IS NULL OR
        execution_state IN ('pending', 'running', 'completed', 'failed', 'interrupted')
    ),
    execution_owner TEXT,
    lease_expires_at BIGINT,
    execution_deadline BIGINT,
    published_at BIGINT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    CHECK (
        (kind = 'thinking' AND tool_id IS NULL AND call_payload IS NULL
         AND execution_state IS NULL AND segment_payload IS NOT NULL)
        OR
        (kind = 'platform' AND tool_id IS NOT NULL AND call_payload IS NOT NULL
         AND execution_state IS NOT NULL AND execution_deadline IS NOT NULL)
    )
);

CREATE INDEX idx_history_markers_principal_reference
ON history_markers(principal, reference);

CREATE INDEX idx_history_markers_execution
ON history_markers(execution_state, lease_expires_at, execution_deadline);

CREATE INDEX idx_history_markers_expiry
ON history_markers(expires_at);

ALTER TABLE models
DROP COLUMN supported_thinking_levels;

ALTER TABLE request_logs ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE request_logs ADD COLUMN thinking_level TEXT;

ALTER TABLE agent_definition_configs
ADD COLUMN thinking_level TEXT
CHECK (thinking_level IN ('off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'));

INSERT INTO model_backends (id, model_id, provider_id, model, weight, priority)
SELECT md5(random()::text || clock_timestamp()::text || models.id),
       models.id,
       models.target_provider,
       models.target_model,
       100,
       1
  FROM models
 WHERE NOT EXISTS (
           SELECT 1 FROM model_backends WHERE model_id = models.id
       );

ALTER TABLE models DROP COLUMN target_provider;
ALTER TABLE models DROP COLUMN target_model;

CREATE UNIQUE INDEX idx_models_route_id ON models(name);

ALTER TABLE web_providers
ADD COLUMN use_proxy BOOLEAN NOT NULL DEFAULT FALSE,
ADD COLUMN local_engines JSONB;

DELETE FROM web_providers
WHERE kind IN ('brave', 'tavily');

ALTER TABLE web_providers
DROP CONSTRAINT web_providers_kind_check,
ALTER COLUMN api_key DROP NOT NULL,
ADD CONSTRAINT web_providers_kind_check
    CHECK (kind IN ('local', 'exa', 'zhipu')),
ADD CONSTRAINT web_providers_credentials_check
    CHECK (
        (
            kind = 'local'
            AND api_key IS NULL
            AND local_engines IS NOT NULL
        )
        OR (
            kind IN ('exa', 'zhipu')
            AND api_key IS NOT NULL
            AND length(trim(api_key)) > 0
            AND local_engines IS NULL
        )
    );

UPDATE web_providers
SET name = name || ' (' || id || ')'
WHERE name = 'Local';

INSERT INTO web_providers (
    id, name, kind, api_key, use_proxy, local_engines
)
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

CREATE UNIQUE INDEX idx_web_providers_local_singleton
ON web_providers(kind)
WHERE kind = 'local';

UPDATE settings AS settings_row
SET value = (
    SELECT COALESCE(
        jsonb_agg(entry.value ORDER BY entry.ordinality),
        '[]'::jsonb
    )::text
    FROM jsonb_array_elements_text(settings_row.value::jsonb)
        WITH ORDINALITY AS entry(value, ordinality)
    JOIN web_providers ON web_providers.id = entry.value
),
updated_at = CURRENT_TIMESTAMP
WHERE settings_row.name IN (
    'web_access_search_provider_ids',
    'web_access_fetch_provider_ids'
);

INSERT INTO settings (name, value)
VALUES
    ('web_access_search_provider_ids', '["web-provider-local"]'),
    ('web_access_fetch_provider_ids', '["web-provider-local"]')
ON CONFLICT (name) DO NOTHING;

UPDATE settings
SET value = '["web-provider-local"]',
    updated_at = CURRENT_TIMESTAMP
WHERE name IN (
    'web_access_search_provider_ids',
    'web_access_fetch_provider_ids'
)
AND value::jsonb = '[]'::jsonb;

CREATE TABLE provider_allowance_samples (
    id              TEXT PRIMARY KEY,
    provider_id     TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    allowance_key   TEXT NOT NULL,
    sampled_at      BIGINT NOT NULL,
    used_value      DOUBLE PRECISION,
    remaining_value DOUBLE PRECISION,
    limit_value     DOUBLE PRECISION,
    used_percent    DOUBLE PRECISION,
    amount_unit     TEXT,
    currency        TEXT,
    reset_at        BIGINT
);

CREATE INDEX idx_provider_allowance_samples_item_time
    ON provider_allowance_samples(provider_id, allowance_key, sampled_at);
CREATE INDEX idx_provider_allowance_samples_sampled_at
    ON provider_allowance_samples(sampled_at);

ALTER TABLE models RENAME COLUMN name TO model_id;
ALTER TABLE models ADD COLUMN display_name TEXT;

ALTER INDEX idx_models_route_id RENAME TO idx_models_route_id_legacy;
CREATE UNIQUE INDEX idx_models_route_id ON models(model_id);
DROP INDEX idx_models_route_id_legacy;

ALTER TABLE model_backends
ADD COLUMN first_token_timeout_ms BIGINT NOT NULL DEFAULT 60000;

ALTER TABLE model_backends
ADD COLUMN target_retry_budget INTEGER NOT NULL DEFAULT 5;

ALTER TABLE model_backends
ADD COLUMN target_cooldown_ms BIGINT NOT NULL DEFAULT 120000;

UPDATE model_backends SET priority = 0;

UPDATE models
SET balance = CASE
    WHEN lower(trim(COALESCE(balance, ''))) = 'latency' THEN 'latency_preference'
    ELSE 'traffic_equalization'
END;

ALTER TABLE models ALTER COLUMN balance SET DEFAULT 'traffic_equalization';
ALTER TABLE model_backends ALTER COLUMN priority SET DEFAULT 0;
ALTER TABLE model_backends DROP COLUMN weight;

ALTER TABLE model_backends
ADD COLUMN enabled BOOLEAN NOT NULL DEFAULT TRUE;
