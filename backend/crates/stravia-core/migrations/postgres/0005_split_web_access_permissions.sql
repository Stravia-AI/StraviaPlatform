ALTER TABLE api_keys
RENAME COLUMN web_access_enabled TO mcp_access_enabled;

ALTER TABLE api_keys
ADD COLUMN web_search_injection_enabled BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE api_keys
SET web_search_injection_enabled = mcp_access_enabled;
