-- 尾部语义指纹与待完成工具索引：按哈希/工具 ID 缩小候选，不因 Principal 总调用数失败。
CREATE TABLE observation_tail_sources (
    run_id TEXT PRIMARY KEY REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    principal TEXT NOT NULL,
    last_unit_hash TEXT NOT NULL,
    generation_node_id TEXT,
    expires_at INTEGER NOT NULL
);
CREATE INDEX observation_tail_sources_hash_idx
    ON observation_tail_sources(principal, last_unit_hash);
CREATE INDEX observation_tail_sources_expiry_idx
    ON observation_tail_sources(expires_at);

CREATE TABLE observation_pending_tools (
    principal TEXT NOT NULL,
    tool_id TEXT NOT NULL,
    run_id TEXT NOT NULL REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (principal, tool_id, run_id)
);
CREATE INDEX observation_pending_tools_lookup_idx
    ON observation_pending_tools(principal, tool_id);
CREATE INDEX observation_pending_tools_expiry_idx
    ON observation_pending_tools(expires_at);
