CREATE TABLE turn_chain_nodes (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('response', 'agent')),
    parent_id       TEXT REFERENCES turn_chain_nodes(id) ON DELETE RESTRICT,
    principal       TEXT NOT NULL,
    payload_version BIGINT NOT NULL CHECK (payload_version > 0),
    payload         TEXT NOT NULL,
    created_at      BIGINT NOT NULL,
    expires_at      BIGINT NOT NULL
);

CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes(principal, kind);
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes(expires_at);
