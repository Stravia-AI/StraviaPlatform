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
