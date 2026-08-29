ALTER TABLE api_keys
ADD COLUMN web_access_enabled INTEGER NOT NULL DEFAULT 0;

CREATE TABLE web_providers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL CHECK (kind IN ('codex', 'exa', 'brave', 'tavily')),
    api_key           TEXT,
    provider_id       TEXT REFERENCES providers(id) ON DELETE CASCADE,
    last_test_success INTEGER,
    last_test_at      TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (
        (kind = 'codex' AND provider_id IS NOT NULL AND api_key IS NULL)
        OR (kind IN ('exa', 'brave', 'tavily') AND api_key IS NOT NULL AND provider_id IS NULL)
    )
);

CREATE INDEX idx_web_providers_provider_id ON web_providers(provider_id);
