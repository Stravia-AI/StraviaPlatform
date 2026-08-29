ALTER TABLE api_keys DROP COLUMN rpm;
ALTER TABLE api_keys DROP COLUMN rpd;
ALTER TABLE api_keys DROP COLUMN tpm;
ALTER TABLE api_keys DROP COLUMN tpd;
ALTER TABLE api_keys
ADD COLUMN concurrency_limit INTEGER CHECK (concurrency_limit > 0);
