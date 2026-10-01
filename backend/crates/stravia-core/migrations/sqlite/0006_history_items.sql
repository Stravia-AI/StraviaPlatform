ALTER TABLE turn_chain_content_refs RENAME TO turn_chain_legacy_refs;
ALTER TABLE turn_chain_contents RENAME TO turn_chain_legacy_contents;
CREATE TABLE turn_chain_nodes_new (
 id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('response','agent','web_search')),
 parent_id TEXT, principal TEXT NOT NULL, payload_version INTEGER NOT NULL CHECK(payload_version>0),
 payload BLOB NOT NULL, legacy_payload TEXT, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
 prefix_namespace TEXT, prefix_fingerprint TEXT, prefix_item_count INTEGER, prefix_completed_at INTEGER,
 storage_format INTEGER NOT NULL DEFAULT 2 CHECK(storage_format IN (0,1,2)),
 UNIQUE(id,principal), UNIQUE(id,principal,kind),
 FOREIGN KEY(parent_id,principal,kind) REFERENCES turn_chain_nodes_new(id,principal,kind) ON DELETE RESTRICT
);
-- The startup converter rewrites payload from legacy_payload in batches, so
-- the table rebuild leaves it empty instead of duplicating every byte twice.
INSERT INTO turn_chain_nodes_new SELECT id,kind,parent_id,principal,payload_version,X'',payload,created_at,expires_at,prefix_namespace,prefix_fingerprint,prefix_item_count,prefix_completed_at,storage_format FROM turn_chain_nodes;
DROP TABLE turn_chain_nodes;
ALTER TABLE turn_chain_nodes_new RENAME TO turn_chain_nodes;
CREATE INDEX idx_turn_chain_expiry ON turn_chain_nodes(expires_at);
CREATE UNIQUE INDEX idx_turn_chain_node_principal ON turn_chain_nodes(id,principal);
CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id);
CREATE INDEX idx_turn_chain_principal_kind ON turn_chain_nodes(principal,kind);
CREATE INDEX idx_turn_chain_reusable_prefix ON turn_chain_nodes(principal,kind,prefix_namespace,prefix_fingerprint,prefix_item_count DESC,prefix_completed_at DESC,expires_at,id DESC) WHERE prefix_namespace IS NOT NULL;
CREATE TABLE turn_chain_contents (
 id INTEGER PRIMARY KEY AUTOINCREMENT, principal TEXT NOT NULL, content_key TEXT NOT NULL, content BLOB NOT NULL,
 UNIQUE(principal, content_key), UNIQUE(principal, id)
);
-- Format-2 nodes keep slot paths inside their binary envelope; this table only
-- records the distinct content ids each node references, for GC.
CREATE TABLE turn_chain_node_contents (
 node_id TEXT NOT NULL, principal TEXT NOT NULL, content_id BIGINT NOT NULL,
 PRIMARY KEY(node_id, content_id),
 FOREIGN KEY(node_id, principal) REFERENCES turn_chain_nodes(id, principal) ON DELETE CASCADE,
 FOREIGN KEY(principal, content_id) REFERENCES turn_chain_contents(principal, id) ON DELETE RESTRICT
) WITHOUT ROWID;
CREATE INDEX idx_turn_chain_node_contents_content_id ON turn_chain_node_contents(principal, content_id);
