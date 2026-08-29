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
