-- Split the saved external binding without changing the original Route or Targets.
CREATE TEMP TABLE route_purpose_search_copy (old_id TEXT PRIMARY KEY, new_id TEXT, new_public_id TEXT) ON COMMIT DROP;
WITH RECURSIVE candidates(old_id, old_public_id, n, new_id, new_public_id) AS (
    SELECT m.id, m.model_id, 0,
           'web-search:' || encode(convert_to(m.id, 'UTF8'), 'hex') || ':0', m.model_id || '-web-search'
    FROM settings s JOIN models m ON m.model_id = CASE
        WHEN pg_input_is_valid(s.value, 'jsonb') THEN s.value::jsonb #>> '{backend,route_id}' END
    WHERE s.name = 'web_search_config'
      AND CASE WHEN pg_input_is_valid(s.value, 'jsonb') THEN s.value::jsonb #>> '{backend,kind}' END = 'external'
    UNION ALL
    SELECT old_id, old_public_id, n + 1,
           'web-search:' || encode(convert_to(old_id, 'UTF8'), 'hex') || ':' || (n + 1),
           old_public_id || '-web-search-' || (n + 1)
    FROM candidates
    WHERE EXISTS (SELECT 1 FROM models WHERE id = candidates.new_id OR model_id = candidates.new_public_id)
)
INSERT INTO route_purpose_search_copy
SELECT old_id, new_id, new_public_id FROM candidates
WHERE NOT EXISTS (SELECT 1 FROM models WHERE id = candidates.new_id OR model_id = candidates.new_public_id);

ALTER TABLE models ADD COLUMN purpose TEXT
    CHECK (purpose IS NULL OR purpose IN ('inference', 'image_generation', 'web_search'));
UPDATE models SET purpose = CASE output_modality WHEN 'text' THEN 'inference' WHEN 'image' THEN 'image_generation' END;
INSERT INTO models (id, model_id, balance, is_enabled, priority, created_at, display_name, default_thinking_level, billing_json, purpose)
SELECT c.new_id, c.new_public_id, m.balance, m.is_enabled, m.priority, m.created_at,
       m.display_name, m.default_thinking_level, m.billing_json, 'web_search'
FROM route_purpose_search_copy c JOIN models m ON m.id = c.old_id;

CREATE TEMP TABLE route_purpose_target_copy (old_id TEXT PRIMARY KEY, new_id TEXT) ON COMMIT DROP;
WITH RECURSIVE candidates(old_id, n, new_id) AS (
    SELECT t.id, 0, 'web-search-target:' || encode(convert_to(t.id, 'UTF8'), 'hex') || ':0'
    FROM model_backends t JOIN route_purpose_search_copy c ON c.old_id = t.model_id
    UNION ALL
    SELECT old_id, n + 1, 'web-search-target:' || encode(convert_to(old_id, 'UTF8'), 'hex') || ':' || (n + 1)
    FROM candidates WHERE EXISTS (SELECT 1 FROM model_backends WHERE id = candidates.new_id)
)
INSERT INTO route_purpose_target_copy
SELECT old_id, new_id FROM candidates
WHERE NOT EXISTS (SELECT 1 FROM model_backends WHERE id = candidates.new_id);

-- Rust trim uses Unicode White_Space; translate, unlike lower(), is ASCII-only.
CREATE TEMP TABLE route_purpose_provider_keys ON COMMIT DROP AS
WITH trimmed AS (
    SELECT p.provider_id, p.model_id, p.metadata_json,
           btrim(p.model_id, U&'\0009\000A\000B\000C\000D\0020\0085\00A0\1680\2000\2001\2002\2003\2004\2005\2006\2007\2008\2009\200A\2028\2029\202F\205F\3000') AS trimmed_id
    FROM (
        SELECT DISTINCT t.provider_id FROM model_backends t
        JOIN route_purpose_search_copy c ON c.old_id = t.model_id
    ) wanted JOIN provider_models p ON p.provider_id = wanted.provider_id
)
SELECT provider_id, model_id, metadata_json,
       translate(reverse(split_part(reverse(trimmed_id), '/', 1)), 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz') AS match_key
FROM trimmed;
CREATE INDEX route_purpose_provider_keys_idx ON route_purpose_provider_keys(provider_id, match_key);
CREATE UNIQUE INDEX route_purpose_provider_exact_idx ON route_purpose_provider_keys(provider_id, model_id);
CREATE TEMP TABLE route_purpose_target_keys ON COMMIT DROP AS
WITH trimmed AS (
    SELECT t.id, t.provider_id, btrim(t.model, U&'\0009\000A\000B\000C\000D\0020\0085\00A0\1680\2000\2001\2002\2003\2004\2005\2006\2007\2008\2009\200A\2028\2029\202F\205F\3000') AS trimmed_id
    FROM model_backends t JOIN route_purpose_search_copy c ON c.old_id = t.model_id
), keyed AS (
    SELECT id, provider_id, trimmed_id,
           translate(reverse(split_part(reverse(trimmed_id), '/', 1)), 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz') AS match_key
    FROM trimmed
)
SELECT k.id, COALESCE(
    (SELECT model_id FROM route_purpose_provider_keys p
     WHERE p.provider_id = k.provider_id AND p.model_id = k.trimmed_id),
    (SELECT MIN(model_id) FROM route_purpose_provider_keys p
     WHERE p.provider_id = k.provider_id AND k.match_key <> '' AND p.match_key = k.match_key
     HAVING COUNT(*) = 1)
) AS matched_model_id
FROM keyed k;

-- Preserve previously skipped Targets as disabled drafts, not newly eligible fallbacks.
INSERT INTO model_backends (id, model_id, provider_id, model, priority, created_at, thinking_level_map,
                            first_token_timeout_ms, target_retry_budget, target_cooldown_ms, enabled)
SELECT tc.new_id, c.new_id, t.provider_id, t.model, t.priority, t.created_at, t.thinking_level_map,
       t.first_token_timeout_ms, t.target_retry_budget, t.target_cooldown_ms,
       CASE WHEN jsonb_typeof(p.metadata_json #> '{modalities,output}') = 'array'
           AND (p.metadata_json #> '{modalities,output}') ? m.output_modality
           THEN t.enabled ELSE false END
FROM model_backends t JOIN route_purpose_target_copy tc ON tc.old_id = t.id
JOIN route_purpose_target_keys tk ON tk.id = t.id
JOIN route_purpose_search_copy c ON c.old_id = t.model_id
JOIN models m ON m.id = c.old_id
LEFT JOIN route_purpose_provider_keys p ON p.provider_id = t.provider_id AND p.model_id = tk.matched_model_id;

UPDATE settings SET value = jsonb_set(value::jsonb, '{backend,route_id}', to_jsonb((SELECT new_public_id FROM route_purpose_search_copy)))::text
WHERE name = 'web_search_config' AND EXISTS (SELECT 1 FROM route_purpose_search_copy);
ALTER TABLE models DROP COLUMN output_modality;
