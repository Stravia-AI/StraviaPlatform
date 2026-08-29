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
