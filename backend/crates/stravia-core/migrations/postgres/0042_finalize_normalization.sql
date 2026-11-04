-- 启动钩子已完成 0041 之后的数据转换；不保留正式双读路径。
DROP INDEX interaction_observations_expiry_idx;
DROP INDEX rejected_requests_expiry_idx;
DROP INDEX observation_events_tool_idx;

ALTER TABLE interaction_observations DROP COLUMN expires_at;
ALTER TABLE interaction_observations DROP COLUMN input_preview;
ALTER TABLE interaction_observations DROP COLUMN visible_tail;

ALTER TABLE inference_run_observations DROP COLUMN expires_at;

ALTER TABLE rejected_request_observations DROP COLUMN expires_at;

ALTER TABLE observation_events DROP COLUMN expires_at;
ALTER TABLE observation_events DROP COLUMN payload;

DROP TABLE observation_normalization_upgrade;
