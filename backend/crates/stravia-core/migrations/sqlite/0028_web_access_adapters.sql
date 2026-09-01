PRAGMA defer_foreign_keys = ON;

ALTER TABLE web_providers RENAME TO web_providers_old;

CREATE TABLE web_providers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL CHECK (kind IN ('local', 'exa', 'zhipu')),
    api_key           TEXT,
    use_proxy         INTEGER NOT NULL DEFAULT 0 CHECK (use_proxy IN (0, 1)),
    local_engines     TEXT,
    last_test_success INTEGER,
    last_test_at      TEXT,
    created_at        TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at        TEXT NOT NULL DEFAULT (datetime('now')),
    CHECK (
        (
            kind = 'local'
            AND api_key IS NULL
            AND local_engines IS NOT NULL
            AND json_valid(local_engines)
        )
        OR (
            kind IN ('exa', 'zhipu')
            AND api_key IS NOT NULL
            AND length(trim(api_key)) > 0
            AND local_engines IS NULL
        )
    )
);

INSERT INTO web_providers (
    id, name, kind, api_key, use_proxy, local_engines,
    last_test_success, last_test_at, created_at, updated_at
)
SELECT
    id, name, kind, api_key, 0, NULL,
    last_test_success, last_test_at, created_at, updated_at
FROM web_providers_old
WHERE kind IN ('exa', 'zhipu');

DROP TABLE web_providers_old;

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
    0,
    json_object(
        'google', json_object('enabled', json('true')),
        'bing', json_object('enabled', json('true')),
        'brave', json_object('enabled', json('true')),
        'baidu', json_object('enabled', json('true')),
        '360', json_object('enabled', json('false')),
        'sogou_weixin', json_object('enabled', json('false')),
        'google_scholar', json_object('enabled', json('false'))
    )
);

CREATE UNIQUE INDEX idx_web_providers_local_singleton
ON web_providers(kind)
WHERE kind = 'local';

UPDATE settings
SET value = CASE
    WHEN json_valid(value) THEN COALESCE(
        (
            SELECT json_group_array(ordered.value)
            FROM (
                SELECT entry.value AS value
                FROM json_each(settings.value) AS entry
                JOIN web_providers ON web_providers.id = entry.value
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

INSERT OR IGNORE INTO settings (name, value)
VALUES
    ('web_access_search_provider_ids', json_array('web-provider-local')),
    ('web_access_fetch_provider_ids', json_array('web-provider-local'));

UPDATE settings
SET value = json_array('web-provider-local'),
    updated_at = datetime('now')
WHERE name IN (
    'web_access_search_provider_ids',
    'web_access_fetch_provider_ids'
)
AND json_array_length(value) = 0;
