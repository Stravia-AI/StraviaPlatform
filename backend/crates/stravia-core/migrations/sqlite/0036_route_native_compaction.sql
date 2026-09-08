ALTER TABLE models ADD COLUMN compaction_enabled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models ADD COLUMN compaction_threshold INTEGER
    CHECK (compaction_threshold IS NULL OR compaction_threshold > 0)
    CHECK (compaction_enabled = 0 OR compaction_threshold IS NOT NULL);
