-- 内容身份在 Principal 内独立；引用随节点删除，不改变历史父边或保留期。
ALTER TABLE turn_chain_nodes ADD COLUMN storage_format INTEGER NOT NULL DEFAULT 0 CHECK (storage_format IN (0, 1));
CREATE UNIQUE INDEX idx_turn_chain_node_principal ON turn_chain_nodes(id, principal);
CREATE TABLE turn_chain_contents (
    principal TEXT NOT NULL,
    content_key TEXT NOT NULL,
    content TEXT NOT NULL,
    PRIMARY KEY (principal, content_key)
);
CREATE TABLE turn_chain_content_refs (
    node_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    path TEXT NOT NULL,
    content_key TEXT NOT NULL,
    PRIMARY KEY (node_id, path),
    FOREIGN KEY (node_id, principal) REFERENCES turn_chain_nodes(id, principal) ON DELETE CASCADE,
    FOREIGN KEY (principal, content_key) REFERENCES turn_chain_contents(principal, content_key)
);
CREATE INDEX idx_turn_chain_content_refs_content ON turn_chain_content_refs(principal, content_key);
