ALTER TABLE web_providers
RENAME TO web_providers_old;

CREATE TABLE web_providers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL CHECK (kind IN ('codex', 'exa', 'brave', 'tavily', 'zhipu')),
    api_key           TEXT,
    provider_id       TEXT REFERENCES providers(id) ON DELETE CASCADE,
    last_test_success INTEGER,
    last_test_at      TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (
        (kind = 'codex' AND provider_id IS NOT NULL AND api_key IS NULL)
        OR (kind IN ('exa', 'brave', 'tavily', 'zhipu') AND api_key IS NOT NULL AND provider_id IS NULL)
    )
);

INSERT INTO web_providers (
    id, name, kind, api_key, provider_id, last_test_success, last_test_at, created_at, updated_at
)
SELECT
    id, name, kind, api_key, provider_id, last_test_success, last_test_at, created_at, updated_at
FROM web_providers_old;

DROP TABLE web_providers_old;

CREATE INDEX idx_web_providers_provider_id ON web_providers(provider_id);
