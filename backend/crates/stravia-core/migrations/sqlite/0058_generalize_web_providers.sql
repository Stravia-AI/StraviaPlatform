CREATE TABLE web_providers_new (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL CHECK (length(trim(kind)) > 0),
    api_key           TEXT CHECK (api_key IS NULL OR length(trim(api_key)) > 0),
    use_proxy         INTEGER NOT NULL DEFAULT 0,
    config_json       TEXT CHECK (config_json IS NULL OR (json_valid(config_json) AND json_type(config_json) = 'object')),
    last_test_success INTEGER,
    last_test_at      TEXT,
    created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO web_providers_new (
    id, name, kind, api_key, use_proxy, config_json,
    last_test_success, last_test_at, created_at, updated_at
)
SELECT
    id, name, kind, api_key, use_proxy, local_engines,
    last_test_success, last_test_at, created_at, updated_at
FROM web_providers
WHERE NOT (id = 'web-provider-local' AND kind = 'local');

DROP TABLE web_providers;
ALTER TABLE web_providers_new RENAME TO web_providers;

CREATE TRIGGER web_providers_boolean_insert BEFORE INSERT ON web_providers
WHEN (typeof(NEW.use_proxy) <> 'integer' OR NEW.use_proxy NOT IN (0, 1))
  OR (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
  OR (NEW.config_json IS NOT NULL AND CASE WHEN json_valid(NEW.config_json) THEN json_type(NEW.config_json) <> 'object' ELSE 1 END)
BEGIN SELECT RAISE(ABORT, 'invalid boolean or provider configuration'); END;

CREATE TRIGGER web_providers_boolean_update BEFORE UPDATE ON web_providers
WHEN (typeof(NEW.use_proxy) <> 'integer' OR NEW.use_proxy NOT IN (0, 1))
  OR (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
  OR (NEW.config_json IS NOT NULL AND CASE WHEN json_valid(NEW.config_json) THEN json_type(NEW.config_json) <> 'object' ELSE 1 END)
BEGIN SELECT RAISE(ABORT, 'invalid boolean or provider configuration'); END;

UPDATE settings
SET value = COALESCE(
    (
        SELECT json_group_array(value)
        FROM (
            SELECT value
            FROM json_each(settings.value)
            WHERE value <> 'web-provider-local'
            ORDER BY key
        )
    ),
    '[]'
)
WHERE name IN ('web_access_search_provider_ids', 'web_access_fetch_provider_ids')
  AND json_valid(value)
  AND json_type(value) = 'array';

DELETE FROM settings WHERE name = 'web_access_browser_path';
