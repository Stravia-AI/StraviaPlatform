-- Keep the original Route and its references; split a saved external search binding.
-- Candidate IDs use encoded internal IDs plus a counter, so collisions never overwrite data.
CREATE TEMP TABLE route_purpose_search_copy (old_id TEXT PRIMARY KEY, new_id TEXT, new_public_id TEXT);
WITH RECURSIVE candidates(old_id, old_public_id, n, new_id, new_public_id) AS (
    SELECT m.id, m.model_id, 0,
           'web-search:' || hex(m.id) || ':0', m.model_id || '-web-search'
    FROM settings s JOIN models m ON m.model_id = CASE
        WHEN json_valid(s.value) THEN json_extract(s.value, '$.backend.route_id') END
    WHERE s.name = 'web_search_config'
      AND CASE WHEN json_valid(s.value) THEN json_extract(s.value, '$.backend.kind') END = 'external'
    UNION ALL
    SELECT old_id, old_public_id, n + 1,
           'web-search:' || hex(old_id) || ':' || (n + 1),
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

CREATE TEMP TABLE route_purpose_target_copy (old_id TEXT PRIMARY KEY, new_id TEXT);
WITH RECURSIVE candidates(old_id, n, new_id) AS (
    SELECT t.id, 0, 'web-search-target:' || hex(t.id) || ':0'
    FROM model_backends t JOIN route_purpose_search_copy c ON c.old_id = t.model_id
    UNION ALL
    SELECT old_id, n + 1, 'web-search-target:' || hex(old_id) || ':' || (n + 1)
    FROM candidates WHERE EXISTS (SELECT 1 FROM model_backends WHERE id = candidates.new_id)
)
INSERT INTO route_purpose_target_copy
SELECT old_id, new_id FROM candidates
WHERE NOT EXISTS (SELECT 1 FROM model_backends WHERE id = candidates.new_id);

-- Match RuntimeConfig::find: exact trimmed ID first, otherwise a unique normalized
-- final path component. SQLite lower() is ASCII-only, matching Rust's ASCII fold.
CREATE TEMP TABLE route_purpose_provider_keys AS
WITH trimmed AS (
    SELECT p.*, trim(p.model_id, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) AS trimmed_id
    FROM (
        SELECT DISTINCT t.provider_id FROM model_backends t
        JOIN route_purpose_search_copy c ON c.old_id = t.model_id
    ) wanted JOIN provider_models p ON p.provider_id = wanted.provider_id
)
SELECT provider_id, model_id, metadata_json,
       lower(substr(trimmed_id, length(rtrim(trimmed_id, replace(trimmed_id, '/', ''))) + 1)) AS match_key
FROM trimmed;
CREATE INDEX route_purpose_provider_keys_idx ON route_purpose_provider_keys(provider_id, match_key);
CREATE UNIQUE INDEX route_purpose_provider_exact_idx ON route_purpose_provider_keys(provider_id, model_id);
CREATE TEMP TABLE route_purpose_target_keys AS
WITH trimmed AS (
    SELECT t.id, t.provider_id, trim(t.model, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) AS trimmed_id
    FROM model_backends t JOIN route_purpose_search_copy c ON c.old_id = t.model_id
), keyed AS (
    SELECT id, provider_id, trimmed_id,
           lower(substr(trimmed_id, length(rtrim(trimmed_id, replace(trimmed_id, '/', ''))) + 1)) AS match_key
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

-- Keep skipped targets as disabled drafts; otherwise dropping the old modality check
-- would make targets that were previously rejected eligible for scheduling.
INSERT INTO model_backends (id, model_id, provider_id, model, priority, created_at, thinking_level_map,
                            first_token_timeout_ms, target_retry_budget, target_cooldown_ms, enabled)
SELECT tc.new_id, c.new_id, t.provider_id, t.model, t.priority, t.created_at, t.thinking_level_map,
       t.first_token_timeout_ms, t.target_retry_budget, t.target_cooldown_ms,
       CASE WHEN json_type(p.metadata_json, '$.modalities.output') = 'array'
           AND EXISTS (SELECT 1 FROM json_each(p.metadata_json, '$.modalities.output') o
                       WHERE o.type = 'text' AND o.value = m.output_modality)
           THEN t.enabled ELSE 0 END
FROM model_backends t JOIN route_purpose_target_copy tc ON tc.old_id = t.id
JOIN route_purpose_target_keys tk ON tk.id = t.id
JOIN route_purpose_search_copy c ON c.old_id = t.model_id
JOIN models m ON m.id = c.old_id
LEFT JOIN route_purpose_provider_keys p ON p.provider_id = t.provider_id AND p.model_id = tk.matched_model_id;

UPDATE settings SET value = json_set(value, '$.backend.route_id', (SELECT new_public_id FROM route_purpose_search_copy))
WHERE name = 'web_search_config' AND EXISTS (SELECT 1 FROM route_purpose_search_copy);
DROP TABLE route_purpose_target_keys;
DROP TABLE route_purpose_provider_keys;
DROP TABLE route_purpose_target_copy;
DROP TABLE route_purpose_search_copy;
ALTER TABLE models DROP COLUMN output_modality;
