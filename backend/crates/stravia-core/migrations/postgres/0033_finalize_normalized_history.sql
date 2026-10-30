-- 启动钩子已验证全部节点、内容、引用和保留期；不保留正式双读路径。
ALTER TABLE native_compactions
    DROP CONSTRAINT native_compactions_source_generation_id_fkey,
    ADD CONSTRAINT native_compactions_source_generation_id_fkey
        FOREIGN KEY (source_generation_id) REFERENCES turn_nodes(id) ON DELETE RESTRICT;
CREATE INDEX native_compactions_generation_idx ON native_compactions(source_generation_id)
    WHERE source_generation_id IS NOT NULL;

DROP TABLE turn_chain_node_contents;
DROP TABLE turn_chain_contents;
DROP TABLE turn_chain_nodes;
DROP TABLE turn_history_upgrade;

-- Token 权威事实在 attempt；读端一直从 attempt 聚合，以下副本没有生产读者。
ALTER TABLE interaction_observations DROP COLUMN input_tokens;
ALTER TABLE interaction_observations DROP COLUMN output_tokens;
ALTER TABLE interaction_observations DROP COLUMN cache_read_tokens;
ALTER TABLE interaction_observations DROP COLUMN cache_write_tokens;
ALTER TABLE interaction_observations DROP COLUMN reasoning_tokens;
ALTER TABLE model_turn_observations DROP COLUMN input_tokens;
ALTER TABLE model_turn_observations DROP COLUMN output_tokens;
ALTER TABLE model_turn_observations DROP COLUMN cache_read_tokens;
ALTER TABLE model_turn_observations DROP COLUMN cache_write_tokens;
ALTER TABLE model_turn_observations DROP COLUMN reasoning_tokens;

-- 原始 turn 分析已由长期统计桶承载；generation 查询不需要 parent 尾列。
DROP INDEX model_turns_analytics_idx;
DROP INDEX interaction_observations_generation_idx;
DROP INDEX rejected_requests_started_idx;
-- 终局错误列表使用缺失 started_at 时的 occurred_at；普通 started_at 索引不能服务此表达式。
CREATE INDEX rejected_requests_client_error_window_idx
    ON rejected_request_observations(COALESCE(started_at, occurred_at) DESC, id)
    WHERE status_code <> 499 AND code NOT IN ('request_aborted', 'cancelled', 'client_disconnected');
DROP INDEX inference_runs_generation_idx;
CREATE INDEX inference_runs_generation_idx ON inference_run_observations(generation_node_id);
