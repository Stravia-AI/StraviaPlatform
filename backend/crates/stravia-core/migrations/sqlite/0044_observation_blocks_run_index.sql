-- observation_blocks.run_id 引用 inference_run_observations(id) ON DELETE CASCADE；
-- 没有以 run_id 开头的索引时，保留期清理每删除一个 Run 都要全表扫描块表，
-- 并在整个交互的清理事务内持有全局写入门。
CREATE INDEX observation_blocks_run_idx ON observation_blocks(run_id);
