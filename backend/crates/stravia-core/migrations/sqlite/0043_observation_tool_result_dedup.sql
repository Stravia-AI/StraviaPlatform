-- Read only requested call IDs along the explicit parent-run lineage.
-- Existing event bodies remain authoritative; no payload copy or history rewrite.
CREATE INDEX idx_observation_events_client_tool_call
    ON observation_events(run_id, json_extract(payload, '$.tool_id'), sequence DESC)
    WHERE kind IN ('client_tool_handoff', 'client_tool_result');
