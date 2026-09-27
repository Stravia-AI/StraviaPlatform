-- 启动恢复按 run 检查未完成的子记录，避免为每条历史 run 重复扫描整张表。
CREATE INDEX model_turns_run_status_idx ON model_turn_observations(run_id, status);
CREATE INDEX target_attempts_run_status_idx ON target_attempt_observations(run_id, status);
