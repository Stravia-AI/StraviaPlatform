-- Drop the threshold first: its CHECK constraint references compaction_enabled.
ALTER TABLE models DROP COLUMN compaction_threshold;
ALTER TABLE models DROP COLUMN compaction_enabled;
