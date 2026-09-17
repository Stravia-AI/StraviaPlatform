-- Non-secret, per-vendor behavior options (e.g. commandcode zdr), stored
-- separately from credentials so they can be returned to clients while
-- adapter_credentials stays write-only.
ALTER TABLE providers ADD COLUMN vendor_options TEXT NOT NULL DEFAULT '{}';
