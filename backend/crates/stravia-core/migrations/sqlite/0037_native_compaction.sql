CREATE TABLE native_compactions (
    id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    source_generation_id TEXT REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT,
    operation_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,
    referenced_at INTEGER,
    expires_at INTEGER NOT NULL
);
CREATE INDEX idx_native_compactions_expiry ON native_compactions(expires_at);
CREATE TABLE native_compaction_states (
    record_id TEXT NOT NULL REFERENCES native_compactions(id) ON DELETE CASCADE,
    principal TEXT NOT NULL,
    native_identity TEXT,
    fingerprint TEXT NOT NULL,
    state_payload TEXT NOT NULL CHECK (json_valid(state_payload)),
    PRIMARY KEY (record_id, fingerprint)
);
CREATE INDEX idx_native_compaction_state_fingerprint ON native_compaction_states(principal, fingerprint);
CREATE INDEX idx_native_compaction_state_identity ON native_compaction_states(principal, native_identity);
CREATE TABLE native_compaction_sources (
    record_id TEXT NOT NULL REFERENCES native_compactions(id) ON DELETE CASCADE,
    source_id TEXT NOT NULL REFERENCES native_compactions(id) ON DELETE RESTRICT,
    PRIMARY KEY (record_id, source_id),
    CHECK (record_id <> source_id)
);
CREATE INDEX idx_native_compaction_source ON native_compaction_sources(source_id);
