ALTER TABLE api_keys
ADD COLUMN transparent_injection_enabled INTEGER NOT NULL DEFAULT 0;

ALTER TABLE api_keys
ADD COLUMN inject_media_understanding INTEGER NOT NULL DEFAULT 0;

ALTER TABLE api_keys
ADD COLUMN inject_web_search INTEGER NOT NULL DEFAULT 0;

UPDATE api_keys
SET transparent_injection_enabled =
        CASE
            WHEN web_search_injection_enabled = 1 OR allow_media_understanding = 1 THEN 1
            ELSE 0
        END,
    inject_media_understanding = allow_media_understanding,
    inject_web_search = web_search_injection_enabled;

ALTER TABLE api_keys DROP COLUMN web_search_injection_enabled;
ALTER TABLE api_keys DROP COLUMN allow_web_research;
ALTER TABLE api_keys DROP COLUMN allow_media_understanding;

UPDATE settings
SET name = 'web_search_config',
    updated_at = datetime('now')
WHERE name = 'web_research_config';

PRAGMA defer_foreign_keys = ON;

ALTER TABLE turn_chain_nodes RENAME TO turn_chain_nodes_old;

CREATE TABLE turn_chain_nodes (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('response', 'agent', 'web_search')),
    parent_id       TEXT REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT,
    principal       TEXT NOT NULL,
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    payload         TEXT NOT NULL CHECK (json_valid(payload)),
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    prefix_namespace    TEXT,
    prefix_fingerprint  TEXT,
    prefix_item_count   INTEGER,
    prefix_completed_at INTEGER
);

INSERT INTO turn_chain_nodes (
    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at,
    prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at
)
SELECT
    id, kind, parent_id, principal, payload_version, payload, created_at, expires_at,
    prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at
FROM turn_chain_nodes_old
WHERE kind <> 'web_research';

UPDATE turn_chain_nodes_old SET parent_id = NULL;

DROP TABLE turn_chain_nodes_old;

CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes(principal, kind);
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes(expires_at);
CREATE INDEX idx_turn_chain_reusable_prefix
ON turn_chain_nodes (
    principal,
    kind,
    prefix_namespace,
    prefix_fingerprint,
    prefix_item_count DESC,
    prefix_completed_at DESC,
    created_at DESC
)
WHERE prefix_namespace IS NOT NULL;
