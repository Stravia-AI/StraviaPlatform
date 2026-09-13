CREATE INDEX IF NOT EXISTS idx_observation_events_context
ON observation_events (interaction_id, sequence)
WHERE kind IN ('compaction_operation', 'native_compaction_associated', 'retained_tail_associated');
