ALTER TABLE web_providers
DROP CONSTRAINT web_providers_kind_check,
DROP CONSTRAINT web_providers_check;

ALTER TABLE web_providers
ADD CONSTRAINT web_providers_kind_check
    CHECK (kind IN ('codex', 'exa', 'brave', 'tavily', 'zhipu')),
ADD CONSTRAINT web_providers_credentials_check
    CHECK (
        (kind = 'codex' AND provider_id IS NOT NULL AND api_key IS NULL)
        OR (kind IN ('exa', 'brave', 'tavily', 'zhipu') AND api_key IS NOT NULL AND provider_id IS NULL)
    );
