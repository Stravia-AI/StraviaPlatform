-- Credential invalidity (ADR-0073): providers.credential_status records the
-- upstream-confirmed rejection of the current credential set.
-- revision is a monotonic write generation used by conditional invalidation
-- writes; updated_at renders to seconds and cannot disambiguate same-second
-- credential changes.
ALTER TABLE providers ADD COLUMN credential_status TEXT NOT NULL DEFAULT 'ok' CHECK (credential_status IN ('ok', 'invalid'));
ALTER TABLE providers ADD COLUMN credential_invalid_at TIMESTAMPTZ;
ALTER TABLE providers ADD COLUMN revision BIGINT NOT NULL DEFAULT 0;
