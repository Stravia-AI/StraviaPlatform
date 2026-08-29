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
