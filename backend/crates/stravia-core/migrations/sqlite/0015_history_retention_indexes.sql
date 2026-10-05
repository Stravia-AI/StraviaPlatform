-- 外键清理必须按完整节点/父边定位，避免在写锁内反复扫描同一 Principal 的全部历史。
DROP INDEX idx_turn_chain_parent;
CREATE INDEX idx_turn_chain_parent ON turn_chain_nodes(parent_id, principal, kind);
CREATE INDEX idx_turn_chain_node_contents_node ON turn_chain_node_contents(node_id, principal);
