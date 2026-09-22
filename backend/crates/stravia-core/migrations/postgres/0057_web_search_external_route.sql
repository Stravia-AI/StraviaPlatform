-- Replace the former direct Codex Provider/model binding with one real Route.
-- Allocate a fresh identity; an administrator may already own any fixed name.
UPDATE settings
SET value = jsonb_set(value::jsonb, '{backend,route_id}', to_jsonb('external-search-' || gen_random_uuid()::text))::text
WHERE name = 'web_search_config'
  AND value::jsonb #>> '{backend,kind}' = 'codex';

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
    value::jsonb #>> '{backend,route_id}',
    value::jsonb #>> '{backend,route_id}',
    'traffic_equalization',
    TRUE,
    0,
    'External Web Search',
    NULL
FROM settings
WHERE name = 'web_search_config'
  AND value::jsonb #>> '{backend,kind}' = 'codex'
  AND btrim(COALESCE(value::jsonb #>> '{backend,provider_id}', '')) <> ''
  AND btrim(COALESCE(value::jsonb #>> '{backend,upstream_model}', '')) <> ''
  AND EXISTS (
      SELECT 1 FROM providers
      WHERE id = settings.value::jsonb #>> '{backend,provider_id}'
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
    (settings.value::jsonb #>> '{backend,route_id}') || '-target',
    models.id,
    settings.value::jsonb #>> '{backend,provider_id}',
    settings.value::jsonb #>> '{backend,upstream_model}',
    0,
    TRUE
FROM settings
JOIN models ON models.model_id = settings.value::jsonb #>> '{backend,route_id}'
WHERE settings.name = 'web_search_config'
  AND settings.value::jsonb #>> '{backend,kind}' = 'codex'
  AND EXISTS (
      SELECT 1 FROM providers
      WHERE id = settings.value::jsonb #>> '{backend,provider_id}'
  );

UPDATE settings
SET value = jsonb_set(
        jsonb_set(
            value::jsonb,
            '{backend}',
            jsonb_build_object('kind', 'external', 'route_id', value::jsonb #>> '{backend,route_id}')
        ),
        '{revision}',
        to_jsonb(COALESCE((value::jsonb ->> 'revision')::bigint, 0) + 1)
    )::text,
    updated_at = CURRENT_TIMESTAMP
WHERE name = 'web_search_config'
  AND value::jsonb #>> '{backend,kind}' = 'codex';

-- Historical external reports remain readable as reports, but the runner
-- rejects their External snapshot before any new upstream call is admitted.
UPDATE turn_chain_nodes
SET payload = jsonb_set(
    payload::jsonb,
    '{snapshot,backend}',
    '{"kind":"external","route_id":"legacy-external-search-history"}'::jsonb
)::text
WHERE kind = 'web_search'
  AND payload::jsonb #>> '{snapshot,backend,kind}' = 'codex';
