-- Replace per-item flags with a JSON array, preserving the canonical item order.
ALTER TABLE api_keys ADD COLUMN transparent_injections jsonb NOT NULL DEFAULT '[]'::jsonb
    CHECK (jsonb_typeof(transparent_injections) = 'array');

UPDATE api_keys SET transparent_injections =
    CASE WHEN inject_web_search THEN '["web_search"]'::jsonb ELSE '[]'::jsonb END ||
    CASE WHEN inject_media_understanding THEN '["media_understanding"]'::jsonb ELSE '[]'::jsonb END ||
    CASE WHEN inject_media_generation THEN '["media_generation"]'::jsonb ELSE '[]'::jsonb END;

ALTER TABLE api_keys DROP COLUMN inject_web_search;
ALTER TABLE api_keys DROP COLUMN inject_media_understanding;
ALTER TABLE api_keys DROP COLUMN inject_media_generation;
