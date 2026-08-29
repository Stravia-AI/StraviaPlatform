ALTER TABLE api_keys
ADD COLUMN allow_web_research INTEGER NOT NULL DEFAULT 0;

UPDATE api_keys
SET allow_web_research = 1
WHERE mcp_access_enabled = 1 OR web_search_injection_enabled = 1;

INSERT OR IGNORE INTO settings (name, value)
SELECT
    'web_research_config',
    json_object(
        'revision', 0,
        'enabled', json('false'),
        'backend', json_object(
            'kind', 'codex',
            'provider_id', provider_id,
            'upstream_model', NULL
        ),
        'max_turns', 12,
        'total_time_seconds', 600,
        'updated_at', ''
    )
FROM web_providers
WHERE kind = 'codex'
  AND (SELECT COUNT(*) FROM web_providers WHERE kind = 'codex') = 1;

UPDATE settings
SET value = CASE
    WHEN json_valid(value) THEN COALESCE(
        (
            SELECT json_group_array(ordered.value)
            FROM (
                SELECT entry.value AS value
                FROM json_each(settings.value) AS entry
                WHERE entry.value NOT IN (
                    SELECT id FROM web_providers WHERE kind = 'codex'
                )
                ORDER BY CAST(entry.key AS INTEGER)
            ) AS ordered
        ),
        '[]'
    )
    ELSE '[]'
END,
updated_at = datetime('now')
WHERE name IN (
    'web_access_search_provider_ids',
    'web_access_fetch_provider_ids'
);

DELETE FROM web_providers WHERE kind = 'codex';

ALTER TABLE web_providers RENAME TO web_providers_old;

CREATE TABLE web_providers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL CHECK (kind IN ('exa', 'brave', 'tavily', 'zhipu')),
    api_key           TEXT NOT NULL,
    last_test_success INTEGER,
    last_test_at      TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO web_providers (
    id, name, kind, api_key, last_test_success, last_test_at, created_at, updated_at
)
SELECT
    id, name, kind, api_key, last_test_success, last_test_at, created_at, updated_at
FROM web_providers_old;

DROP TABLE web_providers_old;


PRAGMA defer_foreign_keys = ON;

ALTER TABLE turn_chain_nodes RENAME TO turn_chain_nodes_old;

CREATE TABLE turn_chain_nodes (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('response', 'agent', 'web_research')),
    parent_id       TEXT REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT,
    principal       TEXT NOT NULL,
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    payload         TEXT NOT NULL CHECK (json_valid(payload)),
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL
);

INSERT INTO turn_chain_nodes (
    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at
)
SELECT
    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at
FROM turn_chain_nodes_old;

UPDATE turn_chain_nodes_old SET parent_id = NULL;

DROP TABLE turn_chain_nodes_old;

CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes(principal, kind);
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes(expires_at);
