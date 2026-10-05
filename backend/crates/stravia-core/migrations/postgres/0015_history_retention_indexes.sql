-- 与 SQLite 保持相同的外键清理索引，保留现有父链和内容引用约束。
DROP INDEX idx_turn_chain_parent;
CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id, principal, kind);
CREATE INDEX idx_turn_chain_node_contents_node ON turn_chain_node_contents(node_id, principal);
