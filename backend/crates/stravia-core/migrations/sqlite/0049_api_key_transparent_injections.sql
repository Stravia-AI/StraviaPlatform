-- Replace per-item flags with a JSON array, preserving the canonical item order.
DROP TRIGGER api_keys_boolean_insert;
DROP TRIGGER api_keys_boolean_update;

ALTER TABLE api_keys ADD COLUMN transparent_injections TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(transparent_injections) AND json_type(transparent_injections) = 'array');

UPDATE api_keys SET transparent_injections = '[' || rtrim(
    CASE WHEN inject_web_search = 1 THEN '"web_search",' ELSE '' END ||
    CASE WHEN inject_media_understanding = 1 THEN '"media_understanding",' ELSE '' END ||
    CASE WHEN inject_media_generation = 1 THEN '"media_generation",' ELSE '' END,
    ','
) || ']';

ALTER TABLE api_keys DROP COLUMN inject_web_search;
ALTER TABLE api_keys DROP COLUMN inject_media_understanding;
ALTER TABLE api_keys DROP COLUMN inject_media_generation;

CREATE TRIGGER api_keys_boolean_insert BEFORE INSERT ON api_keys
WHEN (typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1))
  OR (typeof(NEW.mcp_access_enabled) <> 'integer' OR NEW.mcp_access_enabled NOT IN (0, 1))
  OR (typeof(NEW.transparent_injection_enabled) <> 'integer' OR NEW.transparent_injection_enabled NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;

CREATE TRIGGER api_keys_boolean_update BEFORE UPDATE ON api_keys
WHEN (typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1))
  OR (typeof(NEW.mcp_access_enabled) <> 'integer' OR NEW.mcp_access_enabled NOT IN (0, 1))
  OR (typeof(NEW.transparent_injection_enabled) <> 'integer' OR NEW.transparent_injection_enabled NOT IN (0, 1))
BEGIN SELECT RAISE(ABORT, 'invalid boolean value'); END;
