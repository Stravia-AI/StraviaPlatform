DROP TABLE debug_trace_manifests;
ALTER TABLE rejected_request_observations DROP COLUMN debug_status;
ALTER TABLE inference_run_observations ADD COLUMN delivery_completed_at bigint;
DROP INDEX observation_events_expiry_idx;
DROP INDEX observation_events_interaction_idx;
DROP INDEX observation_events_rejection_idx;
DROP INDEX observation_events_run_idx;
ALTER TABLE observation_events RENAME TO observation_events_legacy;
CREATE TABLE observation_events (
sequence bigint PRIMARY KEY DEFAULT nextval('observation_event_sequence'),
occurred_at bigint NOT NULL,
interaction_id TEXT REFERENCES interaction_observations(id) ON DELETE CASCADE,
run_id TEXT REFERENCES inference_run_observations(id) ON DELETE CASCADE,
rejection_id TEXT REFERENCES rejected_request_observations(id) ON DELETE CASCADE,
kind TEXT NOT NULL, payload bytea NOT NULL, expires_at bigint NOT NULL,
tool_id TEXT, operation_id TEXT
);
CREATE INDEX observation_events_interaction_idx ON observation_events(interaction_id,sequence);
CREATE INDEX observation_events_run_idx ON observation_events(run_id,sequence);
CREATE INDEX observation_events_rejection_idx ON observation_events(rejection_id,sequence) WHERE rejection_id IS NOT NULL;
CREATE INDEX observation_events_tool_idx ON observation_events(tool_id,sequence) WHERE tool_id IS NOT NULL;
CREATE INDEX observation_events_operation_idx ON observation_events(operation_id,sequence) WHERE operation_id IS NOT NULL;
ALTER TABLE observation_events ALTER COLUMN payload SET STORAGE EXTERNAL;
