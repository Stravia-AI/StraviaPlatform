ALTER TABLE provider_oauth_credentials
    ADD COLUMN connection_id TEXT;

UPDATE provider_oauth_credentials
SET connection_id = 'legacy-' || provider_id
WHERE connection_id IS NULL;

ALTER TABLE provider_oauth_credentials
    ALTER COLUMN connection_id SET NOT NULL;

CREATE UNIQUE INDEX idx_oauth_creds_connection_id
    ON provider_oauth_credentials(connection_id);
