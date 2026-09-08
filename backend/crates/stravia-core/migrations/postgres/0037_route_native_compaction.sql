ALTER TABLE models ADD COLUMN compaction_enabled BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE models ADD COLUMN compaction_threshold BIGINT
    CHECK (compaction_threshold IS NULL OR compaction_threshold > 0)
    CHECK (NOT compaction_enabled OR compaction_threshold IS NOT NULL);
