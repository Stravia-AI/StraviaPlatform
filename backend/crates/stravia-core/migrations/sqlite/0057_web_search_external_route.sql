-- Replace the former direct Codex Provider/model binding with one real Route.
-- Allocate a fresh identity; an administrator may already own any fixed name.
UPDATE settings
SET value = json_set(value, '$.backend.route_id', 'external-search-' || lower(hex(randomblob(16))))
WHERE name = 'web_search_config'
  AND json_extract(value, '$.backend.kind') = 'codex';

INSERT INTO models (
    id,
    model_id,
    balance,
    is_enabled,
    priority,
    display_name,
    default_thinking_level
)
SELECT
    json_extract(value, '$.backend.route_id'),
    json_extract(value, '$.backend.route_id'),
    'traffic_equalization',
    1,
    0,
    'External Web Search',
    NULL
FROM settings
WHERE name = 'web_search_config'
  AND json_extract(value, '$.backend.kind') = 'codex'
  AND length(trim(COALESCE(json_extract(value, '$.backend.provider_id'), ''))) > 0
  AND length(trim(COALESCE(json_extract(value, '$.backend.upstream_model'), ''))) > 0
  AND EXISTS (
      SELECT 1 FROM providers
      WHERE id = json_extract(settings.value, '$.backend.provider_id')
  );

INSERT INTO model_backends (
    id,
    model_id,
    provider_id,
    model,
    priority,
    enabled
)
SELECT
    json_extract(settings.value, '$.backend.route_id') || '-target',
    models.id,
    json_extract(settings.value, '$.backend.provider_id'),
    json_extract(settings.value, '$.backend.upstream_model'),
    0,
    1
FROM settings
JOIN models ON models.model_id = json_extract(settings.value, '$.backend.route_id')
WHERE settings.name = 'web_search_config'
  AND json_extract(settings.value, '$.backend.kind') = 'codex'
  AND EXISTS (
      SELECT 1 FROM providers
      WHERE id = json_extract(settings.value, '$.backend.provider_id')
  );

UPDATE settings
SET value = json_set(
        value,
        '$.backend', json_object(
            'kind', 'external',
            'route_id', json_extract(value, '$.backend.route_id')
        ),
        '$.revision', COALESCE(json_extract(value, '$.revision'), 0) + 1
    ),
    updated_at = datetime('now')
WHERE name = 'web_search_config'
  AND json_extract(value, '$.backend.kind') = 'codex';

-- Historical external reports remain readable as reports, but the runner
-- rejects their External snapshot before any new upstream call is admitted.
UPDATE turn_chain_nodes
SET payload = json_set(
    payload,
    '$.snapshot.backend', json_object(
        'kind', 'external',
        'route_id', 'legacy-external-search-history'
    )
)
WHERE kind = 'web_search'
  AND json_extract(payload, '$.snapshot.backend.kind') = 'codex';
