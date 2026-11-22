ALTER TABLE web_providers DROP CONSTRAINT IF EXISTS web_providers_credentials_check;
ALTER TABLE web_providers DROP CONSTRAINT IF EXISTS web_providers_kind_check;
ALTER TABLE web_providers DROP CONSTRAINT IF EXISTS web_providers_local_engines_contract;
DROP INDEX IF EXISTS idx_web_providers_local_singleton;

ALTER TABLE web_providers RENAME COLUMN local_engines TO config_json;

ALTER TABLE web_providers
    ADD CONSTRAINT web_providers_kind_check CHECK (length(trim(kind)) > 0),
    ADD CONSTRAINT web_providers_api_key_check CHECK (api_key IS NULL OR length(trim(api_key)) > 0),
    ADD CONSTRAINT web_providers_config_json_check CHECK (config_json IS NULL OR jsonb_typeof(config_json) = 'object');

DELETE FROM web_providers
WHERE id = 'web-provider-local' AND kind = 'local';

UPDATE settings
SET value = COALESCE(
    (
        SELECT jsonb_agg(elem ORDER BY ord)::text
        FROM jsonb_array_elements_text(settings.value::jsonb) WITH ORDINALITY AS t(elem, ord)
        WHERE elem <> 'web-provider-local'
    ),
    '[]'
)
WHERE name IN ('web_access_search_provider_ids', 'web_access_fetch_provider_ids')
  AND jsonb_typeof(settings.value::jsonb) = 'array';

DELETE FROM settings WHERE name = 'web_access_browser_path';
