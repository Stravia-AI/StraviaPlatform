CREATE TABLE vendor_atomic_provider_fences (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL
);

ALTER TABLE vendor_atomic_lineages ADD COLUMN history_epoch BIGINT NOT NULL DEFAULT 0;
ALTER TABLE vendor_atomic_lineages ADD COLUMN boundary_head TEXT;
ALTER TABLE vendor_atomic_checkpoints ADD COLUMN boundary_head TEXT;
CREATE TABLE vendor_atomic_boundary_facts (
    node_id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    vendor_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    provider_fence BIGINT NOT NULL,
    previous_id TEXT,
    boundary_id TEXT NOT NULL,
    window_id TEXT NOT NULL,
    owner_admission_id TEXT NOT NULL,
    source_admission_id TEXT NOT NULL,
    source_item_gap BIGINT NOT NULL,
    source_unit_gap BIGINT NOT NULL,
    prefix_hash BYTEA NOT NULL,
    controls_hash BYTEA NOT NULL
);
CREATE INDEX vendor_atomic_boundary_facts_previous_idx ON vendor_atomic_boundary_facts(previous_id);
CREATE UNIQUE INDEX vendor_atomic_boundary_facts_candidate_idx ON vendor_atomic_boundary_facts(principal, vendor_id, provider_id, provider_fence, owner_admission_id, boundary_id);
ALTER TABLE vendor_atomic_cells ADD COLUMN journal_head TEXT;
ALTER TABLE vendor_atomic_checkpoint_cells ADD COLUMN journal_head TEXT;
CREATE INDEX vendor_atomic_cells_admission_idx ON vendor_atomic_cells(principal, admission_id);
CREATE INDEX vendor_atomic_checkpoint_cells_provider_idx ON vendor_atomic_checkpoint_cells(provider_id, vendor_id);
CREATE INDEX vendor_atomic_checkpoints_created_idx ON vendor_atomic_checkpoints(created_at_ms);
CREATE INDEX vendor_atomic_lineages_created_idx ON vendor_atomic_lineages(created_at_ms);

CREATE TABLE vendor_atomic_journal_nodes (
    node_id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    vendor_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    data_epoch BIGINT NOT NULL,
    previous_id TEXT,
    content_id BIGINT NOT NULL REFERENCES contents(id),
    logical_size BIGINT NOT NULL CHECK (logical_size >= 0),
    document_size BIGINT NOT NULL CHECK (document_size >= 0)
);
CREATE INDEX vendor_atomic_journal_nodes_content_idx ON vendor_atomic_journal_nodes(content_id);
CREATE INDEX vendor_atomic_journal_nodes_previous_idx ON vendor_atomic_journal_nodes(previous_id);

CREATE TABLE vendor_atomic_journal_pins (
    principal TEXT NOT NULL,
    admission_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    node_id TEXT NOT NULL REFERENCES vendor_atomic_journal_nodes(node_id) ON DELETE CASCADE,
    staged_bytes BIGINT NOT NULL DEFAULT 0 CHECK (staged_bytes >= 0),
    PRIMARY KEY (principal, admission_id, lease_id, node_id)
);
CREATE INDEX vendor_atomic_journal_pins_lease_idx ON vendor_atomic_journal_pins(principal, admission_id, lease_id);
