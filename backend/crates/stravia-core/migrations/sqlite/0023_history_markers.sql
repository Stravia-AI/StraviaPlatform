CREATE TABLE history_markers (
    reference TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('platform', 'thinking')),
    activity TEXT NOT NULL,
    tool_id TEXT,
    call_payload TEXT,
    segment_payload TEXT,
    execution_state TEXT CHECK (
        execution_state IS NULL OR
        execution_state IN ('pending', 'running', 'completed', 'failed', 'interrupted')
    ),
    execution_owner TEXT,
    lease_expires_at INTEGER,
    execution_deadline INTEGER,
    published_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    CHECK (
        (kind = 'thinking' AND tool_id IS NULL AND call_payload IS NULL
         AND execution_state IS NULL AND segment_payload IS NOT NULL)
        OR
        (kind = 'platform' AND tool_id IS NOT NULL AND call_payload IS NOT NULL
         AND execution_state IS NOT NULL AND execution_deadline IS NOT NULL)
    )
);

CREATE INDEX idx_history_markers_principal_reference
ON history_markers(principal, reference);

CREATE INDEX idx_history_markers_execution
ON history_markers(execution_state, lease_expires_at, execution_deadline);

CREATE INDEX idx_history_markers_expiry
ON history_markers(expires_at);
