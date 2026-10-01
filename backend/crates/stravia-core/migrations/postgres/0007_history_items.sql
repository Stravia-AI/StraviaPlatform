ALTER TABLE turn_chain_content_refs RENAME TO turn_chain_legacy_refs;
ALTER TABLE turn_chain_contents RENAME TO turn_chain_legacy_contents;
ALTER TABLE turn_chain_nodes ADD COLUMN legacy_payload TEXT;
-- The startup converter rewrites payload from legacy_payload in batches, so the
-- binary column starts empty instead of duplicating every legacy byte twice.
UPDATE turn_chain_nodes SET legacy_payload = payload;
ALTER TABLE turn_chain_nodes ALTER COLUMN payload TYPE BYTEA USING ''::bytea;
ALTER TABLE turn_chain_nodes ALTER COLUMN payload SET STORAGE EXTERNAL;
ALTER TABLE turn_chain_nodes DROP CONSTRAINT IF EXISTS turn_chain_nodes_storage_format_check;
ALTER TABLE turn_chain_nodes ALTER COLUMN storage_format SET DEFAULT 2;
ALTER TABLE turn_chain_nodes ADD CONSTRAINT turn_chain_nodes_storage_format_v2 CHECK(storage_format IN (0,1,2));
CREATE TABLE turn_chain_contents (
 id BIGSERIAL PRIMARY KEY, principal TEXT NOT NULL, content_key TEXT NOT NULL, content BYTEA NOT NULL,
 UNIQUE(principal, content_key), UNIQUE(principal, id)
);
ALTER TABLE turn_chain_contents ALTER COLUMN content SET STORAGE EXTERNAL;
-- Format-2 nodes keep slot paths inside their binary envelope; this table only
-- records the distinct content ids each node references, for GC.
CREATE TABLE turn_chain_node_contents (
 node_id TEXT NOT NULL, principal TEXT NOT NULL, content_id BIGINT NOT NULL,
 PRIMARY KEY(node_id, content_id),
 FOREIGN KEY(node_id, principal) REFERENCES turn_chain_nodes(id, principal) ON DELETE CASCADE,
 FOREIGN KEY(principal, content_id) REFERENCES turn_chain_contents(principal, id) ON DELETE RESTRICT
);
CREATE INDEX idx_turn_chain_node_contents_content_id ON turn_chain_node_contents(principal, content_id);
