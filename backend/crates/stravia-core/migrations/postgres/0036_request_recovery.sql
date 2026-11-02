-- 客户端请求恢复计数：一次通过率 = 已知覆盖的最终成功请求中未经历自动恢复的部分。
-- 恢复成功必须由同一 Model Turn 内“先失败尝试、后成功尝试”证明；
-- 终态失败天然计为已知未恢复；终态成功只有在明细完整或已有恢复证据时才算已知。
-- 明细缺失（保留期外/观察缺口）保持未知，不把总桶的已终结请求直接当作已知。
-- 统计触发器改为差量刷新：来源重算后只改写发生变化的 checkpoint 行，桶由 stats_usage_delta 按 NEW − OLD 结转；
-- 来源不再产出的行删除 checkpoint，桶由 stats_usage_debit 扣减。不再对每次刷新整行扣减再重新入账。
ALTER TABLE stats_usage_buckets ADD COLUMN recovered_request_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_buckets ADD COLUMN recovery_known_request_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN recovered_request_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE stats_usage_checkpoints ADD COLUMN recovery_known_request_count BIGINT NOT NULL DEFAULT 0;

-- 旧口径 error_count 自 ADR-0083 起不再对外，也不再被任何查询读取，这里一并回收。
-- 触发器函数按名称晚绑定，下方 CREATE OR REPLACE 会把函数体换成不含该列的版本。
ALTER TABLE stats_usage_buckets DROP COLUMN error_count;
ALTER TABLE stats_usage_checkpoints DROP COLUMN error_count;

DROP VIEW stats_usage_turn_source;
CREATE VIEW stats_usage_turn_source AS
SELECT
    'turn' AS source_kind,
    t.id AS source_id,
    'global' AS scope,
    '' AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    NULL AS entity_id,
    '' AS label,
    0 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    0 AS input_tokens_sum,
    0 AS input_tokens_known,
    0 AS output_tokens_sum,
    0 AS output_tokens_known,
    0 AS cache_read_tokens_sum,
    0 AS cache_read_tokens_known,
    0 AS cache_write_tokens_sum,
    0 AS cache_write_tokens_known,
    0 AS reasoning_tokens_sum,
    0 AS reasoning_tokens_known,
    COALESCE(t.finished_at-t.started_at,0) AS duration_ms_sum,
    CASE WHEN t.finished_at IS NOT NULL THEN 1 ELSE 0 END AS duration_ms_count,
    0 AS first_token_ms_sum,
    0 AS first_token_ms_count,
    0 AS successful_attempt_count,
    0 AS successful_output_known_count,
    0 AS successful_duration_known_count,
    0 AS successful_output_sum,
    0 AS successful_duration_sum,
    t.started_at AS last_used_at,
    0 AS cost_custom_nanos,
    0 AS cost_usd_unconverted_nanos,
    0 AS cost_known_count,
    0 AS cost_unknown_count,
    0 AS payg_cost_custom_nanos,
    0 AS payg_cost_usd_unconverted_nanos,
    0 AS payg_cost_known_count,
    0 AS payg_cost_unknown_count,
    0 AS billing_custom_nanos,
    0 AS billing_usd_unconverted_nanos,
    0 AS billing_charged_count,
    0 AS billing_unknown_count
FROM model_turn_observations t
UNION ALL
SELECT
    'turn' AS source_kind,
    t.id AS source_id,
    'model' AS scope,
    t.route_id AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    NULLIF(t.route_id,'') AS entity_id,
    COALESCE(NULLIF(t.model_display_name,''),t.route_id) AS label,
    1 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    0 AS input_tokens_sum,
    0 AS input_tokens_known,
    0 AS output_tokens_sum,
    0 AS output_tokens_known,
    0 AS cache_read_tokens_sum,
    0 AS cache_read_tokens_known,
    0 AS cache_write_tokens_sum,
    0 AS cache_write_tokens_known,
    0 AS reasoning_tokens_sum,
    0 AS reasoning_tokens_known,
    COALESCE(t.finished_at-t.started_at,0) AS duration_ms_sum,
    CASE WHEN t.finished_at IS NOT NULL THEN 1 ELSE 0 END AS duration_ms_count,
    0 AS first_token_ms_sum,
    0 AS first_token_ms_count,
    0 AS successful_attempt_count,
    0 AS successful_output_known_count,
    0 AS successful_duration_known_count,
    0 AS successful_output_sum,
    0 AS successful_duration_sum,
    t.started_at AS last_used_at,
    0 AS cost_custom_nanos,
    0 AS cost_usd_unconverted_nanos,
    0 AS cost_known_count,
    0 AS cost_unknown_count,
    0 AS payg_cost_custom_nanos,
    0 AS payg_cost_usd_unconverted_nanos,
    0 AS payg_cost_known_count,
    0 AS payg_cost_unknown_count,
    0 AS billing_custom_nanos,
    0 AS billing_usd_unconverted_nanos,
    0 AS billing_charged_count,
    0 AS billing_unknown_count
FROM model_turn_observations t
UNION ALL
SELECT
    'turn' AS source_kind,
    t.id AS source_id,
    'api_key' AS scope,
    COALESCE(t.api_key_id,'') AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    t.api_key_id AS entity_id,
    COALESCE(NULLIF(t.api_key_name,''),t.api_key_id,'Unknown API Key') AS label,
    1 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    0 AS input_tokens_sum,
    0 AS input_tokens_known,
    0 AS output_tokens_sum,
    0 AS output_tokens_known,
    0 AS cache_read_tokens_sum,
    0 AS cache_read_tokens_known,
    0 AS cache_write_tokens_sum,
    0 AS cache_write_tokens_known,
    0 AS reasoning_tokens_sum,
    0 AS reasoning_tokens_known,
    COALESCE(t.finished_at-t.started_at,0) AS duration_ms_sum,
    CASE WHEN t.finished_at IS NOT NULL THEN 1 ELSE 0 END AS duration_ms_count,
    0 AS first_token_ms_sum,
    0 AS first_token_ms_count,
    0 AS successful_attempt_count,
    0 AS successful_output_known_count,
    0 AS successful_duration_known_count,
    0 AS successful_output_sum,
    0 AS successful_duration_sum,
    t.started_at AS last_used_at,
    0 AS cost_custom_nanos,
    0 AS cost_usd_unconverted_nanos,
    0 AS cost_known_count,
    0 AS cost_unknown_count,
    0 AS payg_cost_custom_nanos,
    0 AS payg_cost_usd_unconverted_nanos,
    0 AS payg_cost_known_count,
    0 AS payg_cost_unknown_count,
    0 AS billing_custom_nanos,
    0 AS billing_usd_unconverted_nanos,
    0 AS billing_charged_count,
    0 AS billing_unknown_count
FROM model_turn_observations t;

DROP VIEW stats_usage_attempt_source;
CREATE VIEW stats_usage_attempt_source AS
SELECT
    'attempt' AS source_kind,
    a.id AS source_id,
    'global' AS scope,
    '' AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    NULL AS entity_id,
    '' AS label,
    0 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    CASE WHEN a.status='completed' OR (a.status='failed' AND a.error_source='upstream') THEN 1 ELSE 0 END AS upstream_request_count,
    CASE WHEN a.status='failed' AND a.error_source='upstream' THEN 1 ELSE 0 END AS upstream_error_count,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END ELSE 0 END AS input_tokens_sum,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS input_tokens_known,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN a.output_tokens ELSE 0 END AS output_tokens_sum,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS output_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN a.cache_read_tokens ELSE 0 END AS cache_read_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_read_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN a.cache_write_tokens ELSE 0 END AS cache_write_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_write_tokens_known,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN a.reasoning_tokens ELSE 0 END AS reasoning_tokens_sum,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN 1 ELSE 0 END AS reasoning_tokens_known,
    0 AS duration_ms_sum,
    0 AS duration_ms_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.first_token_ms,0) ELSE 0 END AS first_token_ms_sum,
    CASE WHEN a.status='completed' AND a.first_token_ms IS NOT NULL THEN 1 ELSE 0 END AS first_token_ms_count,
    CASE WHEN a.status='completed' THEN 1 ELSE 0 END AS successful_attempt_count,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS successful_output_known_count,
    CASE WHEN a.status='completed' AND a.duration_ms IS NOT NULL THEN 1 ELSE 0 END AS successful_duration_known_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.output_tokens,0) ELSE 0 END AS successful_output_sum,
    CASE WHEN a.status='completed' THEN COALESCE(a.duration_ms,0) ELSE 0 END AS successful_duration_sum,
    t.started_at AS last_used_at,
    CASE WHEN a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS cost_custom_nanos,
    CASE WHEN a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS cost_usd_unconverted_nanos,
    CASE WHEN a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis IS NOT NULL AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS cost_unknown_count,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS payg_cost_custom_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS payg_cost_usd_unconverted_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS payg_cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis='usage' AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS payg_cost_unknown_count,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NOT NULL THEN a.billing_custom_nanos ELSE 0 END AS billing_custom_nanos,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NULL THEN COALESCE(a.billing_usd_nanos,0) ELSE 0 END AS billing_usd_unconverted_nanos,
    CASE WHEN a.billing_state='charged' THEN 1 ELSE 0 END AS billing_charged_count,
    CASE WHEN a.billing_state='unknown' THEN 1 ELSE 0 END AS billing_unknown_count
FROM target_attempt_observations a JOIN model_turn_observations t ON t.id=a.model_turn_id
UNION ALL
SELECT
    'attempt' AS source_kind,
    a.id AS source_id,
    'model' AS scope,
    t.route_id AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    NULLIF(t.route_id,'') AS entity_id,
    COALESCE(NULLIF(t.model_display_name,''),t.route_id) AS label,
    0 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END ELSE 0 END AS input_tokens_sum,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS input_tokens_known,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN a.output_tokens ELSE 0 END AS output_tokens_sum,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS output_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN a.cache_read_tokens ELSE 0 END AS cache_read_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_read_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN a.cache_write_tokens ELSE 0 END AS cache_write_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_write_tokens_known,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN a.reasoning_tokens ELSE 0 END AS reasoning_tokens_sum,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN 1 ELSE 0 END AS reasoning_tokens_known,
    0 AS duration_ms_sum,
    0 AS duration_ms_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.first_token_ms,0) ELSE 0 END AS first_token_ms_sum,
    CASE WHEN a.status='completed' AND a.first_token_ms IS NOT NULL THEN 1 ELSE 0 END AS first_token_ms_count,
    CASE WHEN a.status='completed' THEN 1 ELSE 0 END AS successful_attempt_count,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS successful_output_known_count,
    CASE WHEN a.status='completed' AND a.duration_ms IS NOT NULL THEN 1 ELSE 0 END AS successful_duration_known_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.output_tokens,0) ELSE 0 END AS successful_output_sum,
    CASE WHEN a.status='completed' THEN COALESCE(a.duration_ms,0) ELSE 0 END AS successful_duration_sum,
    t.started_at AS last_used_at,
    CASE WHEN a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS cost_custom_nanos,
    CASE WHEN a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS cost_usd_unconverted_nanos,
    CASE WHEN a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis IS NOT NULL AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS cost_unknown_count,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS payg_cost_custom_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS payg_cost_usd_unconverted_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS payg_cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis='usage' AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS payg_cost_unknown_count,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NOT NULL THEN a.billing_custom_nanos ELSE 0 END AS billing_custom_nanos,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NULL THEN COALESCE(a.billing_usd_nanos,0) ELSE 0 END AS billing_usd_unconverted_nanos,
    CASE WHEN a.billing_state='charged' THEN 1 ELSE 0 END AS billing_charged_count,
    CASE WHEN a.billing_state='unknown' THEN 1 ELSE 0 END AS billing_unknown_count
FROM target_attempt_observations a JOIN model_turn_observations t ON t.id=a.model_turn_id
UNION ALL
SELECT
    'attempt' AS source_kind,
    a.id AS source_id,
    'api_key' AS scope,
    COALESCE(t.api_key_id,'') AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    t.api_key_id AS entity_id,
    COALESCE(NULLIF(t.api_key_name,''),t.api_key_id,'Unknown API Key') AS label,
    0 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END ELSE 0 END AS input_tokens_sum,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS input_tokens_known,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN a.output_tokens ELSE 0 END AS output_tokens_sum,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS output_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN a.cache_read_tokens ELSE 0 END AS cache_read_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_read_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN a.cache_write_tokens ELSE 0 END AS cache_write_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_write_tokens_known,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN a.reasoning_tokens ELSE 0 END AS reasoning_tokens_sum,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN 1 ELSE 0 END AS reasoning_tokens_known,
    0 AS duration_ms_sum,
    0 AS duration_ms_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.first_token_ms,0) ELSE 0 END AS first_token_ms_sum,
    CASE WHEN a.status='completed' AND a.first_token_ms IS NOT NULL THEN 1 ELSE 0 END AS first_token_ms_count,
    CASE WHEN a.status='completed' THEN 1 ELSE 0 END AS successful_attempt_count,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS successful_output_known_count,
    CASE WHEN a.status='completed' AND a.duration_ms IS NOT NULL THEN 1 ELSE 0 END AS successful_duration_known_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.output_tokens,0) ELSE 0 END AS successful_output_sum,
    CASE WHEN a.status='completed' THEN COALESCE(a.duration_ms,0) ELSE 0 END AS successful_duration_sum,
    t.started_at AS last_used_at,
    CASE WHEN a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS cost_custom_nanos,
    CASE WHEN a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS cost_usd_unconverted_nanos,
    CASE WHEN a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis IS NOT NULL AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS cost_unknown_count,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS payg_cost_custom_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS payg_cost_usd_unconverted_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS payg_cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis='usage' AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS payg_cost_unknown_count,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NOT NULL THEN a.billing_custom_nanos ELSE 0 END AS billing_custom_nanos,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NULL THEN COALESCE(a.billing_usd_nanos,0) ELSE 0 END AS billing_usd_unconverted_nanos,
    CASE WHEN a.billing_state='charged' THEN 1 ELSE 0 END AS billing_charged_count,
    CASE WHEN a.billing_state='unknown' THEN 1 ELSE 0 END AS billing_unknown_count
FROM target_attempt_observations a JOIN model_turn_observations t ON t.id=a.model_turn_id
UNION ALL
SELECT
    'attempt' AS source_kind,
    a.id AS source_id,
    'provider' AS scope,
    a.provider_id AS identity,
    (t.started_at / 900000) * 900000 AS bucket_start,
    NULLIF(a.provider_id,'') AS entity_id,
    COALESCE(NULLIF(a.provider_name,''),a.provider_id) AS label,
    0 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    CASE WHEN a.status='completed' OR (a.status='failed' AND a.error_source='upstream') THEN 1 ELSE 0 END AS upstream_request_count,
    CASE WHEN a.status='failed' AND a.error_source='upstream' THEN 1 ELSE 0 END AS upstream_error_count,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN CASE WHEN a.input_tokens>a.cache_read_tokens THEN a.input_tokens-a.cache_read_tokens ELSE 0 END ELSE 0 END AS input_tokens_sum,
    CASE WHEN a.status='completed' AND a.input_tokens IS NOT NULL AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS input_tokens_known,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN a.output_tokens ELSE 0 END AS output_tokens_sum,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS output_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN a.cache_read_tokens ELSE 0 END AS cache_read_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_read_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_read_tokens_known,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN a.cache_write_tokens ELSE 0 END AS cache_write_tokens_sum,
    CASE WHEN a.status='completed' AND a.cache_write_tokens IS NOT NULL THEN 1 ELSE 0 END AS cache_write_tokens_known,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN a.reasoning_tokens ELSE 0 END AS reasoning_tokens_sum,
    CASE WHEN a.status='completed' AND a.reasoning_tokens IS NOT NULL THEN 1 ELSE 0 END AS reasoning_tokens_known,
    COALESCE(a.duration_ms,0) AS duration_ms_sum,
    CASE WHEN a.duration_ms IS NOT NULL THEN 1 ELSE 0 END AS duration_ms_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.first_token_ms,0) ELSE 0 END AS first_token_ms_sum,
    CASE WHEN a.status='completed' AND a.first_token_ms IS NOT NULL THEN 1 ELSE 0 END AS first_token_ms_count,
    CASE WHEN a.status='completed' THEN 1 ELSE 0 END AS successful_attempt_count,
    CASE WHEN a.status='completed' AND a.output_tokens IS NOT NULL THEN 1 ELSE 0 END AS successful_output_known_count,
    CASE WHEN a.status='completed' AND a.duration_ms IS NOT NULL THEN 1 ELSE 0 END AS successful_duration_known_count,
    CASE WHEN a.status='completed' THEN COALESCE(a.output_tokens,0) ELSE 0 END AS successful_output_sum,
    CASE WHEN a.status='completed' THEN COALESCE(a.duration_ms,0) ELSE 0 END AS successful_duration_sum,
    t.started_at AS last_used_at,
    CASE WHEN a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS cost_custom_nanos,
    CASE WHEN a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS cost_usd_unconverted_nanos,
    CASE WHEN a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis IS NOT NULL AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS cost_unknown_count,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NOT NULL THEN a.cost_custom_nanos ELSE 0 END AS payg_cost_custom_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_custom_nanos IS NULL THEN COALESCE(a.cost_usd_nanos,0) ELSE 0 END AS payg_cost_usd_unconverted_nanos,
    CASE WHEN a.cost_basis='usage' AND a.cost_usd_nanos IS NOT NULL THEN 1 ELSE 0 END AS payg_cost_known_count,
    CASE WHEN a.status <> 'running' AND a.cost_basis='usage' AND a.cost_usd_nanos IS NULL THEN 1 ELSE 0 END AS payg_cost_unknown_count,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NOT NULL THEN a.billing_custom_nanos ELSE 0 END AS billing_custom_nanos,
    CASE WHEN a.billing_state='charged' AND a.billing_custom_nanos IS NULL THEN COALESCE(a.billing_usd_nanos,0) ELSE 0 END AS billing_usd_unconverted_nanos,
    CASE WHEN a.billing_state='charged' THEN 1 ELSE 0 END AS billing_charged_count,
    CASE WHEN a.billing_state='unknown' THEN 1 ELSE 0 END AS billing_unknown_count
FROM target_attempt_observations a JOIN model_turn_observations t ON t.id=a.model_turn_id;

DROP VIEW IF EXISTS stats_usage_run_recovery_source;
CREATE VIEW stats_usage_run_recovery_source AS
SELECT
    run_id,
    CASE WHEN terminal AND NOT failed AND recovery_evidence THEN 1 ELSE 0 END AS recovered_request_count,
    CASE WHEN terminal AND (failed OR recovery_evidence OR complete_evidence) THEN 1 ELSE 0 END AS recovery_known_request_count
FROM (
    SELECT
        r.id AS run_id,
        (r.status IN ('completed','failed','waiting_client','superseded') AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) AND NOT r.rejected_before_target) AS terminal,
        (r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) AND NOT r.rejected_before_target) AS failed,
        EXISTS (
            SELECT 1
            FROM target_attempt_observations AS failed_attempt
            JOIN target_attempt_observations AS retry_attempt
              ON retry_attempt.model_turn_id = failed_attempt.model_turn_id
             AND retry_attempt.id <> failed_attempt.id
             AND retry_attempt.status = 'completed'
             AND retry_attempt.started_at >= failed_attempt.finished_at
            WHERE failed_attempt.run_id = r.id
              AND failed_attempt.status = 'failed'
              AND failed_attempt.finished_at IS NOT NULL
              AND retry_attempt.finished_at IS NOT NULL
              AND retry_attempt.finished_at <= r.finished_at
        ) AS recovery_evidence,
        (COALESCE(i.observation_gap, TRUE) = FALSE
         AND EXISTS (SELECT 1 FROM target_attempt_observations AS observed WHERE observed.run_id = r.id AND observed.started_at <= r.finished_at)
         AND NOT EXISTS (SELECT 1 FROM target_attempt_observations AS pending WHERE pending.run_id = r.id AND pending.status IN ('running','interrupted') AND pending.started_at <= r.finished_at)) AS complete_evidence
    FROM inference_run_observations AS r
    LEFT JOIN interaction_observations AS i ON i.id = r.interaction_id
) AS facts;

DROP VIEW stats_usage_run_source;
CREATE VIEW stats_usage_run_source AS
SELECT
    'run' AS source_kind,
    r.id AS source_id,
    'global' AS scope,
    '' AS identity,
    (r.started_at / 900000) * 900000 AS bucket_start,
    NULL AS entity_id,
    '' AS label,
    1 AS request_count,
    CASE WHEN r.status IN ('completed','failed','waiting_client','superseded') AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) AND NOT r.rejected_before_target THEN 1 ELSE 0 END AS terminal_request_count,
    CASE WHEN r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) AND NOT r.rejected_before_target THEN 1 ELSE 0 END AS failed_request_count,
    recovery.recovered_request_count,
    recovery.recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    0 AS input_tokens_sum,
    0 AS input_tokens_known,
    0 AS output_tokens_sum,
    0 AS output_tokens_known,
    0 AS cache_read_tokens_sum,
    0 AS cache_read_tokens_known,
    0 AS cache_write_tokens_sum,
    0 AS cache_write_tokens_known,
    0 AS reasoning_tokens_sum,
    0 AS reasoning_tokens_known,
    0 AS duration_ms_sum,
    0 AS duration_ms_count,
    0 AS first_token_ms_sum,
    0 AS first_token_ms_count,
    0 AS successful_attempt_count,
    0 AS successful_output_known_count,
    0 AS successful_duration_known_count,
    0 AS successful_output_sum,
    0 AS successful_duration_sum,
    r.started_at AS last_used_at,
    0 AS cost_custom_nanos,
    0 AS cost_usd_unconverted_nanos,
    0 AS cost_known_count,
    0 AS cost_unknown_count,
    0 AS payg_cost_custom_nanos,
    0 AS payg_cost_usd_unconverted_nanos,
    0 AS payg_cost_known_count,
    0 AS payg_cost_unknown_count,
    0 AS billing_custom_nanos,
    0 AS billing_usd_unconverted_nanos,
    0 AS billing_charged_count,
    0 AS billing_unknown_count
FROM inference_run_observations r
JOIN stats_usage_run_recovery_source AS recovery ON recovery.run_id = r.id
UNION ALL
SELECT
    'run' AS source_kind,
    r.id AS source_id,
    'provider' AS scope,
    p.provider_id AS identity,
    (r.started_at / 900000) * 900000 AS bucket_start,
    NULLIF(p.provider_id,'') AS entity_id,
    COALESCE(NULLIF(p.provider_name,''),p.provider_id) AS label,
    1 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    0 AS input_tokens_sum,
    0 AS input_tokens_known,
    0 AS output_tokens_sum,
    0 AS output_tokens_known,
    0 AS cache_read_tokens_sum,
    0 AS cache_read_tokens_known,
    0 AS cache_write_tokens_sum,
    0 AS cache_write_tokens_known,
    0 AS reasoning_tokens_sum,
    0 AS reasoning_tokens_known,
    0 AS duration_ms_sum,
    0 AS duration_ms_count,
    0 AS first_token_ms_sum,
    0 AS first_token_ms_count,
    0 AS successful_attempt_count,
    0 AS successful_output_known_count,
    0 AS successful_duration_known_count,
    0 AS successful_output_sum,
    0 AS successful_duration_sum,
    r.started_at AS last_used_at,
    0 AS cost_custom_nanos,
    0 AS cost_usd_unconverted_nanos,
    0 AS cost_known_count,
    0 AS cost_unknown_count,
    0 AS payg_cost_custom_nanos,
    0 AS payg_cost_usd_unconverted_nanos,
    0 AS payg_cost_known_count,
    0 AS payg_cost_unknown_count,
    0 AS billing_custom_nanos,
    0 AS billing_usd_unconverted_nanos,
    0 AS billing_charged_count,
    0 AS billing_unknown_count
FROM inference_run_observations r JOIN (SELECT run_id,provider_id,MAX(provider_name) AS provider_name FROM target_attempt_observations GROUP BY run_id,provider_id) p ON p.run_id=r.id;

DROP VIEW stats_usage_rejection_source;
CREATE VIEW stats_usage_rejection_source AS
SELECT
    'rejection' AS source_kind,
    r.id AS source_id,
    'global' AS scope,
    '' AS identity,
    (COALESCE(r.started_at,r.occurred_at) / 900000) * 900000 AS bucket_start,
    NULL AS entity_id,
    '' AS label,
    1 AS request_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
    0 AS recovered_request_count,
    0 AS recovery_known_request_count,
    0 AS upstream_request_count,
    0 AS upstream_error_count,
    0 AS input_tokens_sum,
    0 AS input_tokens_known,
    0 AS output_tokens_sum,
    0 AS output_tokens_known,
    0 AS cache_read_tokens_sum,
    0 AS cache_read_tokens_known,
    0 AS cache_write_tokens_sum,
    0 AS cache_write_tokens_known,
    0 AS reasoning_tokens_sum,
    0 AS reasoning_tokens_known,
    0 AS duration_ms_sum,
    0 AS duration_ms_count,
    0 AS first_token_ms_sum,
    0 AS first_token_ms_count,
    0 AS successful_attempt_count,
    0 AS successful_output_known_count,
    0 AS successful_duration_known_count,
    0 AS successful_output_sum,
    0 AS successful_duration_sum,
    COALESCE(r.started_at,r.occurred_at) AS last_used_at,
    0 AS cost_custom_nanos,
    0 AS cost_usd_unconverted_nanos,
    0 AS cost_known_count,
    0 AS cost_unknown_count,
    0 AS payg_cost_custom_nanos,
    0 AS payg_cost_usd_unconverted_nanos,
    0 AS payg_cost_known_count,
    0 AS payg_cost_unknown_count,
    0 AS billing_custom_nanos,
    0 AS billing_usd_unconverted_nanos,
    0 AS billing_charged_count,
    0 AS billing_unknown_count
FROM rejected_request_observations r;

CREATE OR REPLACE FUNCTION stats_usage_credit() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
INSERT INTO stats_usage_buckets (
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    terminal_request_count,
    failed_request_count,
    recovered_request_count,
    recovery_known_request_count,
    upstream_request_count,
    upstream_error_count,
    input_tokens_sum,
    input_tokens_known,
    output_tokens_sum,
    output_tokens_known,
    cache_read_tokens_sum,
    cache_read_tokens_known,
    cache_write_tokens_sum,
    cache_write_tokens_known,
    reasoning_tokens_sum,
    reasoning_tokens_known,
    duration_ms_sum,
    duration_ms_count,
    first_token_ms_sum,
    first_token_ms_count,
    successful_attempt_count,
    successful_output_known_count,
    successful_duration_known_count,
    successful_output_sum,
    successful_duration_sum,
    last_used_at,
    cost_custom_nanos,
    cost_usd_unconverted_nanos,
    cost_known_count,
    cost_unknown_count,
    payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos,
    payg_cost_known_count,
    payg_cost_unknown_count,
    billing_custom_nanos,
    billing_usd_unconverted_nanos,
    billing_charged_count,
    billing_unknown_count
) VALUES (
    NEW.scope,
    NEW.identity,
    NEW.bucket_start,
    NEW.entity_id,
    NEW.label,
    NEW.request_count,
    NEW.terminal_request_count,
    NEW.failed_request_count,
    NEW.recovered_request_count,
    NEW.recovery_known_request_count,
    NEW.upstream_request_count,
    NEW.upstream_error_count,
    NEW.input_tokens_sum,
    NEW.input_tokens_known,
    NEW.output_tokens_sum,
    NEW.output_tokens_known,
    NEW.cache_read_tokens_sum,
    NEW.cache_read_tokens_known,
    NEW.cache_write_tokens_sum,
    NEW.cache_write_tokens_known,
    NEW.reasoning_tokens_sum,
    NEW.reasoning_tokens_known,
    NEW.duration_ms_sum,
    NEW.duration_ms_count,
    NEW.first_token_ms_sum,
    NEW.first_token_ms_count,
    NEW.successful_attempt_count,
    NEW.successful_output_known_count,
    NEW.successful_duration_known_count,
    NEW.successful_output_sum,
    NEW.successful_duration_sum,
    NEW.last_used_at,
    NEW.cost_custom_nanos,
    NEW.cost_usd_unconverted_nanos,
    NEW.cost_known_count,
    NEW.cost_unknown_count,
    NEW.payg_cost_custom_nanos,
    NEW.payg_cost_usd_unconverted_nanos,
    NEW.payg_cost_known_count,
    NEW.payg_cost_unknown_count,
    NEW.billing_custom_nanos,
    NEW.billing_usd_unconverted_nanos,
    NEW.billing_charged_count,
    NEW.billing_unknown_count
) ON CONFLICT (scope, identity, bucket_start) DO UPDATE SET
    request_count = stats_usage_buckets.request_count + excluded.request_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count + excluded.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count + excluded.failed_request_count,
    recovered_request_count = stats_usage_buckets.recovered_request_count + excluded.recovered_request_count,
    recovery_known_request_count = stats_usage_buckets.recovery_known_request_count + excluded.recovery_known_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count + excluded.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count + excluded.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum + excluded.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known + excluded.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum + excluded.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known + excluded.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum + excluded.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known + excluded.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum + excluded.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known + excluded.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum + excluded.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known + excluded.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum + excluded.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count + excluded.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum + excluded.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count + excluded.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count + excluded.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count + excluded.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count + excluded.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum + excluded.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum + excluded.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos + excluded.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos + excluded.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count + excluded.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count + excluded.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos + excluded.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos + excluded.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count + excluded.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count + excluded.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos + excluded.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos + excluded.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count + excluded.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count + excluded.billing_unknown_count,
    label = excluded.label,
    last_used_at = GREATEST(stats_usage_buckets.last_used_at, excluded.last_used_at);
    RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION stats_usage_turn_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        -- Reclaim work, but preserve permanent historical contribution.
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'turn' AND source_id = OLD.id;
        RETURN OLD;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        -- 来源不再产出的行：删除 checkpoint，由 stats_usage_debit 从长期桶扣除。
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'turn' AND source_id = NEW.id
          AND (scope, identity, bucket_start) NOT IN (
            SELECT s.scope, s.identity, s.bucket_start FROM stats_usage_turn_source AS s WHERE s.source_id = NEW.id
          );
    END IF;
-- 只改写发生变化的 checkpoint 行；桶的增量由 stats_usage_delta 按 NEW − OLD 结转。
INSERT INTO stats_usage_checkpoints (
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
)
SELECT
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
FROM stats_usage_turn_source
WHERE source_id = NEW.id
ON CONFLICT (source_kind, source_id, scope, identity, bucket_start) DO UPDATE SET
    (
        entity_id, label, request_count, terminal_request_count, failed_request_count,
        recovered_request_count, recovery_known_request_count, upstream_request_count,
        upstream_error_count, input_tokens_sum, input_tokens_known, output_tokens_sum,
        output_tokens_known, cache_read_tokens_sum, cache_read_tokens_known, cache_write_tokens_sum,
        cache_write_tokens_known, reasoning_tokens_sum, reasoning_tokens_known, duration_ms_sum,
        duration_ms_count, first_token_ms_sum, first_token_ms_count, successful_attempt_count,
        successful_output_known_count, successful_duration_known_count, successful_output_sum,
        successful_duration_sum, last_used_at, cost_custom_nanos, cost_usd_unconverted_nanos,
        cost_known_count, cost_unknown_count, payg_cost_custom_nanos,
        payg_cost_usd_unconverted_nanos, payg_cost_known_count, payg_cost_unknown_count,
        billing_custom_nanos, billing_usd_unconverted_nanos, billing_charged_count,
        billing_unknown_count
    ) = (
        EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
        EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
        EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
        EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
        EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
        EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
        EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
        EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
        EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count,
        EXCLUDED.successful_attempt_count, EXCLUDED.successful_output_known_count,
        EXCLUDED.successful_duration_known_count, EXCLUDED.successful_output_sum,
        EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at, EXCLUDED.cost_custom_nanos,
        EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count, EXCLUDED.cost_unknown_count,
        EXCLUDED.payg_cost_custom_nanos, EXCLUDED.payg_cost_usd_unconverted_nanos,
        EXCLUDED.payg_cost_known_count, EXCLUDED.payg_cost_unknown_count,
        EXCLUDED.billing_custom_nanos, EXCLUDED.billing_usd_unconverted_nanos,
        EXCLUDED.billing_charged_count, EXCLUDED.billing_unknown_count
    )
WHERE (
    stats_usage_checkpoints.entity_id, stats_usage_checkpoints.label,
    stats_usage_checkpoints.request_count, stats_usage_checkpoints.terminal_request_count,
    stats_usage_checkpoints.failed_request_count, stats_usage_checkpoints.recovered_request_count,
    stats_usage_checkpoints.recovery_known_request_count,
    stats_usage_checkpoints.upstream_request_count, stats_usage_checkpoints.upstream_error_count,
    stats_usage_checkpoints.input_tokens_sum, stats_usage_checkpoints.input_tokens_known,
    stats_usage_checkpoints.output_tokens_sum, stats_usage_checkpoints.output_tokens_known,
    stats_usage_checkpoints.cache_read_tokens_sum, stats_usage_checkpoints.cache_read_tokens_known,
    stats_usage_checkpoints.cache_write_tokens_sum,
    stats_usage_checkpoints.cache_write_tokens_known, stats_usage_checkpoints.reasoning_tokens_sum,
    stats_usage_checkpoints.reasoning_tokens_known, stats_usage_checkpoints.duration_ms_sum,
    stats_usage_checkpoints.duration_ms_count, stats_usage_checkpoints.first_token_ms_sum,
    stats_usage_checkpoints.first_token_ms_count, stats_usage_checkpoints.successful_attempt_count,
    stats_usage_checkpoints.successful_output_known_count,
    stats_usage_checkpoints.successful_duration_known_count,
    stats_usage_checkpoints.successful_output_sum, stats_usage_checkpoints.successful_duration_sum,
    stats_usage_checkpoints.last_used_at, stats_usage_checkpoints.cost_custom_nanos,
    stats_usage_checkpoints.cost_usd_unconverted_nanos, stats_usage_checkpoints.cost_known_count,
    stats_usage_checkpoints.cost_unknown_count, stats_usage_checkpoints.payg_cost_custom_nanos,
    stats_usage_checkpoints.payg_cost_usd_unconverted_nanos,
    stats_usage_checkpoints.payg_cost_known_count, stats_usage_checkpoints.payg_cost_unknown_count,
    stats_usage_checkpoints.billing_custom_nanos,
    stats_usage_checkpoints.billing_usd_unconverted_nanos,
    stats_usage_checkpoints.billing_charged_count, stats_usage_checkpoints.billing_unknown_count
) IS DISTINCT FROM (
    EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
    EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
    EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
    EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
    EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
    EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
    EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
    EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
    EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count, EXCLUDED.successful_attempt_count,
    EXCLUDED.successful_output_known_count, EXCLUDED.successful_duration_known_count,
    EXCLUDED.successful_output_sum, EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at,
    EXCLUDED.cost_custom_nanos, EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count,
    EXCLUDED.cost_unknown_count, EXCLUDED.payg_cost_custom_nanos,
    EXCLUDED.payg_cost_usd_unconverted_nanos, EXCLUDED.payg_cost_known_count,
    EXCLUDED.payg_cost_unknown_count, EXCLUDED.billing_custom_nanos,
    EXCLUDED.billing_usd_unconverted_nanos, EXCLUDED.billing_charged_count,
    EXCLUDED.billing_unknown_count
);
    RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION stats_usage_attempt_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        -- Reclaim work, but preserve permanent historical contribution.
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'attempt' AND source_id = OLD.id;
        RETURN OLD;
    END IF;
-- Serialize provider request revisions with the owning run.
PERFORM 1 FROM inference_run_observations WHERE id = NEW.run_id FOR UPDATE;
    IF TG_OP = 'UPDATE' THEN
        -- 来源不再产出的行：删除 checkpoint，由 stats_usage_debit 从长期桶扣除。
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'attempt' AND source_id = NEW.id
          AND (scope, identity, bucket_start) NOT IN (
            SELECT s.scope, s.identity, s.bucket_start FROM stats_usage_attempt_source AS s WHERE s.source_id = NEW.id
          );
    END IF;
-- 只改写发生变化的 checkpoint 行；桶的增量由 stats_usage_delta 按 NEW − OLD 结转。
INSERT INTO stats_usage_checkpoints (
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
)
SELECT
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
FROM stats_usage_attempt_source
WHERE source_id = NEW.id
ON CONFLICT (source_kind, source_id, scope, identity, bucket_start) DO UPDATE SET
    (
        entity_id, label, request_count, terminal_request_count, failed_request_count,
        recovered_request_count, recovery_known_request_count, upstream_request_count,
        upstream_error_count, input_tokens_sum, input_tokens_known, output_tokens_sum,
        output_tokens_known, cache_read_tokens_sum, cache_read_tokens_known, cache_write_tokens_sum,
        cache_write_tokens_known, reasoning_tokens_sum, reasoning_tokens_known, duration_ms_sum,
        duration_ms_count, first_token_ms_sum, first_token_ms_count, successful_attempt_count,
        successful_output_known_count, successful_duration_known_count, successful_output_sum,
        successful_duration_sum, last_used_at, cost_custom_nanos, cost_usd_unconverted_nanos,
        cost_known_count, cost_unknown_count, payg_cost_custom_nanos,
        payg_cost_usd_unconverted_nanos, payg_cost_known_count, payg_cost_unknown_count,
        billing_custom_nanos, billing_usd_unconverted_nanos, billing_charged_count,
        billing_unknown_count
    ) = (
        EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
        EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
        EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
        EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
        EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
        EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
        EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
        EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
        EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count,
        EXCLUDED.successful_attempt_count, EXCLUDED.successful_output_known_count,
        EXCLUDED.successful_duration_known_count, EXCLUDED.successful_output_sum,
        EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at, EXCLUDED.cost_custom_nanos,
        EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count, EXCLUDED.cost_unknown_count,
        EXCLUDED.payg_cost_custom_nanos, EXCLUDED.payg_cost_usd_unconverted_nanos,
        EXCLUDED.payg_cost_known_count, EXCLUDED.payg_cost_unknown_count,
        EXCLUDED.billing_custom_nanos, EXCLUDED.billing_usd_unconverted_nanos,
        EXCLUDED.billing_charged_count, EXCLUDED.billing_unknown_count
    )
WHERE (
    stats_usage_checkpoints.entity_id, stats_usage_checkpoints.label,
    stats_usage_checkpoints.request_count, stats_usage_checkpoints.terminal_request_count,
    stats_usage_checkpoints.failed_request_count, stats_usage_checkpoints.recovered_request_count,
    stats_usage_checkpoints.recovery_known_request_count,
    stats_usage_checkpoints.upstream_request_count, stats_usage_checkpoints.upstream_error_count,
    stats_usage_checkpoints.input_tokens_sum, stats_usage_checkpoints.input_tokens_known,
    stats_usage_checkpoints.output_tokens_sum, stats_usage_checkpoints.output_tokens_known,
    stats_usage_checkpoints.cache_read_tokens_sum, stats_usage_checkpoints.cache_read_tokens_known,
    stats_usage_checkpoints.cache_write_tokens_sum,
    stats_usage_checkpoints.cache_write_tokens_known, stats_usage_checkpoints.reasoning_tokens_sum,
    stats_usage_checkpoints.reasoning_tokens_known, stats_usage_checkpoints.duration_ms_sum,
    stats_usage_checkpoints.duration_ms_count, stats_usage_checkpoints.first_token_ms_sum,
    stats_usage_checkpoints.first_token_ms_count, stats_usage_checkpoints.successful_attempt_count,
    stats_usage_checkpoints.successful_output_known_count,
    stats_usage_checkpoints.successful_duration_known_count,
    stats_usage_checkpoints.successful_output_sum, stats_usage_checkpoints.successful_duration_sum,
    stats_usage_checkpoints.last_used_at, stats_usage_checkpoints.cost_custom_nanos,
    stats_usage_checkpoints.cost_usd_unconverted_nanos, stats_usage_checkpoints.cost_known_count,
    stats_usage_checkpoints.cost_unknown_count, stats_usage_checkpoints.payg_cost_custom_nanos,
    stats_usage_checkpoints.payg_cost_usd_unconverted_nanos,
    stats_usage_checkpoints.payg_cost_known_count, stats_usage_checkpoints.payg_cost_unknown_count,
    stats_usage_checkpoints.billing_custom_nanos,
    stats_usage_checkpoints.billing_usd_unconverted_nanos,
    stats_usage_checkpoints.billing_charged_count, stats_usage_checkpoints.billing_unknown_count
) IS DISTINCT FROM (
    EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
    EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
    EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
    EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
    EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
    EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
    EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
    EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
    EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count, EXCLUDED.successful_attempt_count,
    EXCLUDED.successful_output_known_count, EXCLUDED.successful_duration_known_count,
    EXCLUDED.successful_output_sum, EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at,
    EXCLUDED.cost_custom_nanos, EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count,
    EXCLUDED.cost_unknown_count, EXCLUDED.payg_cost_custom_nanos,
    EXCLUDED.payg_cost_usd_unconverted_nanos, EXCLUDED.payg_cost_known_count,
    EXCLUDED.payg_cost_unknown_count, EXCLUDED.billing_custom_nanos,
    EXCLUDED.billing_usd_unconverted_nanos, EXCLUDED.billing_charged_count,
    EXCLUDED.billing_unknown_count
);
-- 来源不再产出的行：删除 checkpoint，由 stats_usage_debit 从长期桶扣除。
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'run' AND source_id = NEW.run_id
  AND (scope, identity, bucket_start) NOT IN (
    SELECT s.scope, s.identity, s.bucket_start FROM stats_usage_run_source AS s WHERE s.source_id = NEW.run_id
  );
-- 只改写发生变化的 checkpoint 行；桶的增量由 stats_usage_delta 按 NEW − OLD 结转。
INSERT INTO stats_usage_checkpoints (
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
)
SELECT
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
FROM stats_usage_run_source
WHERE source_id = NEW.run_id
ON CONFLICT (source_kind, source_id, scope, identity, bucket_start) DO UPDATE SET
    (
        entity_id, label, request_count, terminal_request_count, failed_request_count,
        recovered_request_count, recovery_known_request_count, upstream_request_count,
        upstream_error_count, input_tokens_sum, input_tokens_known, output_tokens_sum,
        output_tokens_known, cache_read_tokens_sum, cache_read_tokens_known, cache_write_tokens_sum,
        cache_write_tokens_known, reasoning_tokens_sum, reasoning_tokens_known, duration_ms_sum,
        duration_ms_count, first_token_ms_sum, first_token_ms_count, successful_attempt_count,
        successful_output_known_count, successful_duration_known_count, successful_output_sum,
        successful_duration_sum, last_used_at, cost_custom_nanos, cost_usd_unconverted_nanos,
        cost_known_count, cost_unknown_count, payg_cost_custom_nanos,
        payg_cost_usd_unconverted_nanos, payg_cost_known_count, payg_cost_unknown_count,
        billing_custom_nanos, billing_usd_unconverted_nanos, billing_charged_count,
        billing_unknown_count
    ) = (
        EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
        EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
        EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
        EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
        EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
        EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
        EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
        EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
        EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count,
        EXCLUDED.successful_attempt_count, EXCLUDED.successful_output_known_count,
        EXCLUDED.successful_duration_known_count, EXCLUDED.successful_output_sum,
        EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at, EXCLUDED.cost_custom_nanos,
        EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count, EXCLUDED.cost_unknown_count,
        EXCLUDED.payg_cost_custom_nanos, EXCLUDED.payg_cost_usd_unconverted_nanos,
        EXCLUDED.payg_cost_known_count, EXCLUDED.payg_cost_unknown_count,
        EXCLUDED.billing_custom_nanos, EXCLUDED.billing_usd_unconverted_nanos,
        EXCLUDED.billing_charged_count, EXCLUDED.billing_unknown_count
    )
WHERE (
    stats_usage_checkpoints.entity_id, stats_usage_checkpoints.label,
    stats_usage_checkpoints.request_count, stats_usage_checkpoints.terminal_request_count,
    stats_usage_checkpoints.failed_request_count, stats_usage_checkpoints.recovered_request_count,
    stats_usage_checkpoints.recovery_known_request_count,
    stats_usage_checkpoints.upstream_request_count, stats_usage_checkpoints.upstream_error_count,
    stats_usage_checkpoints.input_tokens_sum, stats_usage_checkpoints.input_tokens_known,
    stats_usage_checkpoints.output_tokens_sum, stats_usage_checkpoints.output_tokens_known,
    stats_usage_checkpoints.cache_read_tokens_sum, stats_usage_checkpoints.cache_read_tokens_known,
    stats_usage_checkpoints.cache_write_tokens_sum,
    stats_usage_checkpoints.cache_write_tokens_known, stats_usage_checkpoints.reasoning_tokens_sum,
    stats_usage_checkpoints.reasoning_tokens_known, stats_usage_checkpoints.duration_ms_sum,
    stats_usage_checkpoints.duration_ms_count, stats_usage_checkpoints.first_token_ms_sum,
    stats_usage_checkpoints.first_token_ms_count, stats_usage_checkpoints.successful_attempt_count,
    stats_usage_checkpoints.successful_output_known_count,
    stats_usage_checkpoints.successful_duration_known_count,
    stats_usage_checkpoints.successful_output_sum, stats_usage_checkpoints.successful_duration_sum,
    stats_usage_checkpoints.last_used_at, stats_usage_checkpoints.cost_custom_nanos,
    stats_usage_checkpoints.cost_usd_unconverted_nanos, stats_usage_checkpoints.cost_known_count,
    stats_usage_checkpoints.cost_unknown_count, stats_usage_checkpoints.payg_cost_custom_nanos,
    stats_usage_checkpoints.payg_cost_usd_unconverted_nanos,
    stats_usage_checkpoints.payg_cost_known_count, stats_usage_checkpoints.payg_cost_unknown_count,
    stats_usage_checkpoints.billing_custom_nanos,
    stats_usage_checkpoints.billing_usd_unconverted_nanos,
    stats_usage_checkpoints.billing_charged_count, stats_usage_checkpoints.billing_unknown_count
) IS DISTINCT FROM (
    EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
    EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
    EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
    EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
    EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
    EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
    EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
    EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
    EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count, EXCLUDED.successful_attempt_count,
    EXCLUDED.successful_output_known_count, EXCLUDED.successful_duration_known_count,
    EXCLUDED.successful_output_sum, EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at,
    EXCLUDED.cost_custom_nanos, EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count,
    EXCLUDED.cost_unknown_count, EXCLUDED.payg_cost_custom_nanos,
    EXCLUDED.payg_cost_usd_unconverted_nanos, EXCLUDED.payg_cost_known_count,
    EXCLUDED.payg_cost_unknown_count, EXCLUDED.billing_custom_nanos,
    EXCLUDED.billing_usd_unconverted_nanos, EXCLUDED.billing_charged_count,
    EXCLUDED.billing_unknown_count
);
    RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION stats_usage_run_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        -- Reclaim work, but preserve permanent historical contribution.
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'run' AND source_id = OLD.id;
        RETURN OLD;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        -- 来源不再产出的行：删除 checkpoint，由 stats_usage_debit 从长期桶扣除。
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'run' AND source_id = NEW.id
          AND (scope, identity, bucket_start) NOT IN (
            SELECT s.scope, s.identity, s.bucket_start FROM stats_usage_run_source AS s WHERE s.source_id = NEW.id
          );
    END IF;
-- 只改写发生变化的 checkpoint 行；桶的增量由 stats_usage_delta 按 NEW − OLD 结转。
INSERT INTO stats_usage_checkpoints (
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
)
SELECT
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
FROM stats_usage_run_source
WHERE source_id = NEW.id
ON CONFLICT (source_kind, source_id, scope, identity, bucket_start) DO UPDATE SET
    (
        entity_id, label, request_count, terminal_request_count, failed_request_count,
        recovered_request_count, recovery_known_request_count, upstream_request_count,
        upstream_error_count, input_tokens_sum, input_tokens_known, output_tokens_sum,
        output_tokens_known, cache_read_tokens_sum, cache_read_tokens_known, cache_write_tokens_sum,
        cache_write_tokens_known, reasoning_tokens_sum, reasoning_tokens_known, duration_ms_sum,
        duration_ms_count, first_token_ms_sum, first_token_ms_count, successful_attempt_count,
        successful_output_known_count, successful_duration_known_count, successful_output_sum,
        successful_duration_sum, last_used_at, cost_custom_nanos, cost_usd_unconverted_nanos,
        cost_known_count, cost_unknown_count, payg_cost_custom_nanos,
        payg_cost_usd_unconverted_nanos, payg_cost_known_count, payg_cost_unknown_count,
        billing_custom_nanos, billing_usd_unconverted_nanos, billing_charged_count,
        billing_unknown_count
    ) = (
        EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
        EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
        EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
        EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
        EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
        EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
        EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
        EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
        EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count,
        EXCLUDED.successful_attempt_count, EXCLUDED.successful_output_known_count,
        EXCLUDED.successful_duration_known_count, EXCLUDED.successful_output_sum,
        EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at, EXCLUDED.cost_custom_nanos,
        EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count, EXCLUDED.cost_unknown_count,
        EXCLUDED.payg_cost_custom_nanos, EXCLUDED.payg_cost_usd_unconverted_nanos,
        EXCLUDED.payg_cost_known_count, EXCLUDED.payg_cost_unknown_count,
        EXCLUDED.billing_custom_nanos, EXCLUDED.billing_usd_unconverted_nanos,
        EXCLUDED.billing_charged_count, EXCLUDED.billing_unknown_count
    )
WHERE (
    stats_usage_checkpoints.entity_id, stats_usage_checkpoints.label,
    stats_usage_checkpoints.request_count, stats_usage_checkpoints.terminal_request_count,
    stats_usage_checkpoints.failed_request_count, stats_usage_checkpoints.recovered_request_count,
    stats_usage_checkpoints.recovery_known_request_count,
    stats_usage_checkpoints.upstream_request_count, stats_usage_checkpoints.upstream_error_count,
    stats_usage_checkpoints.input_tokens_sum, stats_usage_checkpoints.input_tokens_known,
    stats_usage_checkpoints.output_tokens_sum, stats_usage_checkpoints.output_tokens_known,
    stats_usage_checkpoints.cache_read_tokens_sum, stats_usage_checkpoints.cache_read_tokens_known,
    stats_usage_checkpoints.cache_write_tokens_sum,
    stats_usage_checkpoints.cache_write_tokens_known, stats_usage_checkpoints.reasoning_tokens_sum,
    stats_usage_checkpoints.reasoning_tokens_known, stats_usage_checkpoints.duration_ms_sum,
    stats_usage_checkpoints.duration_ms_count, stats_usage_checkpoints.first_token_ms_sum,
    stats_usage_checkpoints.first_token_ms_count, stats_usage_checkpoints.successful_attempt_count,
    stats_usage_checkpoints.successful_output_known_count,
    stats_usage_checkpoints.successful_duration_known_count,
    stats_usage_checkpoints.successful_output_sum, stats_usage_checkpoints.successful_duration_sum,
    stats_usage_checkpoints.last_used_at, stats_usage_checkpoints.cost_custom_nanos,
    stats_usage_checkpoints.cost_usd_unconverted_nanos, stats_usage_checkpoints.cost_known_count,
    stats_usage_checkpoints.cost_unknown_count, stats_usage_checkpoints.payg_cost_custom_nanos,
    stats_usage_checkpoints.payg_cost_usd_unconverted_nanos,
    stats_usage_checkpoints.payg_cost_known_count, stats_usage_checkpoints.payg_cost_unknown_count,
    stats_usage_checkpoints.billing_custom_nanos,
    stats_usage_checkpoints.billing_usd_unconverted_nanos,
    stats_usage_checkpoints.billing_charged_count, stats_usage_checkpoints.billing_unknown_count
) IS DISTINCT FROM (
    EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
    EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
    EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
    EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
    EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
    EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
    EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
    EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
    EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count, EXCLUDED.successful_attempt_count,
    EXCLUDED.successful_output_known_count, EXCLUDED.successful_duration_known_count,
    EXCLUDED.successful_output_sum, EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at,
    EXCLUDED.cost_custom_nanos, EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count,
    EXCLUDED.cost_unknown_count, EXCLUDED.payg_cost_custom_nanos,
    EXCLUDED.payg_cost_usd_unconverted_nanos, EXCLUDED.payg_cost_known_count,
    EXCLUDED.payg_cost_unknown_count, EXCLUDED.billing_custom_nanos,
    EXCLUDED.billing_usd_unconverted_nanos, EXCLUDED.billing_charged_count,
    EXCLUDED.billing_unknown_count
);
    RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION stats_usage_rejection_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        -- Reclaim work, but preserve permanent historical contribution.
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'rejection' AND source_id = OLD.id;
        RETURN OLD;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        -- 来源不再产出的行：删除 checkpoint，由 stats_usage_debit 从长期桶扣除。
        DELETE FROM stats_usage_checkpoints
        WHERE source_kind = 'rejection' AND source_id = NEW.id
          AND (scope, identity, bucket_start) NOT IN (
            SELECT s.scope, s.identity, s.bucket_start FROM stats_usage_rejection_source AS s WHERE s.source_id = NEW.id
          );
    END IF;
-- 只改写发生变化的 checkpoint 行；桶的增量由 stats_usage_delta 按 NEW − OLD 结转。
INSERT INTO stats_usage_checkpoints (
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
)
SELECT
    source_kind, source_id, scope, identity, bucket_start, entity_id, label, request_count,
    terminal_request_count, failed_request_count, recovered_request_count,
    recovery_known_request_count, upstream_request_count, upstream_error_count, input_tokens_sum,
    input_tokens_known, output_tokens_sum, output_tokens_known, cache_read_tokens_sum,
    cache_read_tokens_known, cache_write_tokens_sum, cache_write_tokens_known, reasoning_tokens_sum,
    reasoning_tokens_known, duration_ms_sum, duration_ms_count, first_token_ms_sum,
    first_token_ms_count, successful_attempt_count, successful_output_known_count,
    successful_duration_known_count, successful_output_sum, successful_duration_sum, last_used_at,
    cost_custom_nanos, cost_usd_unconverted_nanos, cost_known_count, cost_unknown_count,
    payg_cost_custom_nanos, payg_cost_usd_unconverted_nanos, payg_cost_known_count,
    payg_cost_unknown_count, billing_custom_nanos, billing_usd_unconverted_nanos,
    billing_charged_count, billing_unknown_count
FROM stats_usage_rejection_source
WHERE source_id = NEW.id
ON CONFLICT (source_kind, source_id, scope, identity, bucket_start) DO UPDATE SET
    (
        entity_id, label, request_count, terminal_request_count, failed_request_count,
        recovered_request_count, recovery_known_request_count, upstream_request_count,
        upstream_error_count, input_tokens_sum, input_tokens_known, output_tokens_sum,
        output_tokens_known, cache_read_tokens_sum, cache_read_tokens_known, cache_write_tokens_sum,
        cache_write_tokens_known, reasoning_tokens_sum, reasoning_tokens_known, duration_ms_sum,
        duration_ms_count, first_token_ms_sum, first_token_ms_count, successful_attempt_count,
        successful_output_known_count, successful_duration_known_count, successful_output_sum,
        successful_duration_sum, last_used_at, cost_custom_nanos, cost_usd_unconverted_nanos,
        cost_known_count, cost_unknown_count, payg_cost_custom_nanos,
        payg_cost_usd_unconverted_nanos, payg_cost_known_count, payg_cost_unknown_count,
        billing_custom_nanos, billing_usd_unconverted_nanos, billing_charged_count,
        billing_unknown_count
    ) = (
        EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
        EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
        EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
        EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
        EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
        EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
        EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
        EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
        EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count,
        EXCLUDED.successful_attempt_count, EXCLUDED.successful_output_known_count,
        EXCLUDED.successful_duration_known_count, EXCLUDED.successful_output_sum,
        EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at, EXCLUDED.cost_custom_nanos,
        EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count, EXCLUDED.cost_unknown_count,
        EXCLUDED.payg_cost_custom_nanos, EXCLUDED.payg_cost_usd_unconverted_nanos,
        EXCLUDED.payg_cost_known_count, EXCLUDED.payg_cost_unknown_count,
        EXCLUDED.billing_custom_nanos, EXCLUDED.billing_usd_unconverted_nanos,
        EXCLUDED.billing_charged_count, EXCLUDED.billing_unknown_count
    )
WHERE (
    stats_usage_checkpoints.entity_id, stats_usage_checkpoints.label,
    stats_usage_checkpoints.request_count, stats_usage_checkpoints.terminal_request_count,
    stats_usage_checkpoints.failed_request_count, stats_usage_checkpoints.recovered_request_count,
    stats_usage_checkpoints.recovery_known_request_count,
    stats_usage_checkpoints.upstream_request_count, stats_usage_checkpoints.upstream_error_count,
    stats_usage_checkpoints.input_tokens_sum, stats_usage_checkpoints.input_tokens_known,
    stats_usage_checkpoints.output_tokens_sum, stats_usage_checkpoints.output_tokens_known,
    stats_usage_checkpoints.cache_read_tokens_sum, stats_usage_checkpoints.cache_read_tokens_known,
    stats_usage_checkpoints.cache_write_tokens_sum,
    stats_usage_checkpoints.cache_write_tokens_known, stats_usage_checkpoints.reasoning_tokens_sum,
    stats_usage_checkpoints.reasoning_tokens_known, stats_usage_checkpoints.duration_ms_sum,
    stats_usage_checkpoints.duration_ms_count, stats_usage_checkpoints.first_token_ms_sum,
    stats_usage_checkpoints.first_token_ms_count, stats_usage_checkpoints.successful_attempt_count,
    stats_usage_checkpoints.successful_output_known_count,
    stats_usage_checkpoints.successful_duration_known_count,
    stats_usage_checkpoints.successful_output_sum, stats_usage_checkpoints.successful_duration_sum,
    stats_usage_checkpoints.last_used_at, stats_usage_checkpoints.cost_custom_nanos,
    stats_usage_checkpoints.cost_usd_unconverted_nanos, stats_usage_checkpoints.cost_known_count,
    stats_usage_checkpoints.cost_unknown_count, stats_usage_checkpoints.payg_cost_custom_nanos,
    stats_usage_checkpoints.payg_cost_usd_unconverted_nanos,
    stats_usage_checkpoints.payg_cost_known_count, stats_usage_checkpoints.payg_cost_unknown_count,
    stats_usage_checkpoints.billing_custom_nanos,
    stats_usage_checkpoints.billing_usd_unconverted_nanos,
    stats_usage_checkpoints.billing_charged_count, stats_usage_checkpoints.billing_unknown_count
) IS DISTINCT FROM (
    EXCLUDED.entity_id, EXCLUDED.label, EXCLUDED.request_count, EXCLUDED.terminal_request_count,
    EXCLUDED.failed_request_count, EXCLUDED.recovered_request_count,
    EXCLUDED.recovery_known_request_count, EXCLUDED.upstream_request_count,
    EXCLUDED.upstream_error_count, EXCLUDED.input_tokens_sum, EXCLUDED.input_tokens_known,
    EXCLUDED.output_tokens_sum, EXCLUDED.output_tokens_known, EXCLUDED.cache_read_tokens_sum,
    EXCLUDED.cache_read_tokens_known, EXCLUDED.cache_write_tokens_sum,
    EXCLUDED.cache_write_tokens_known, EXCLUDED.reasoning_tokens_sum,
    EXCLUDED.reasoning_tokens_known, EXCLUDED.duration_ms_sum, EXCLUDED.duration_ms_count,
    EXCLUDED.first_token_ms_sum, EXCLUDED.first_token_ms_count, EXCLUDED.successful_attempt_count,
    EXCLUDED.successful_output_known_count, EXCLUDED.successful_duration_known_count,
    EXCLUDED.successful_output_sum, EXCLUDED.successful_duration_sum, EXCLUDED.last_used_at,
    EXCLUDED.cost_custom_nanos, EXCLUDED.cost_usd_unconverted_nanos, EXCLUDED.cost_known_count,
    EXCLUDED.cost_unknown_count, EXCLUDED.payg_cost_custom_nanos,
    EXCLUDED.payg_cost_usd_unconverted_nanos, EXCLUDED.payg_cost_known_count,
    EXCLUDED.payg_cost_unknown_count, EXCLUDED.billing_custom_nanos,
    EXCLUDED.billing_usd_unconverted_nanos, EXCLUDED.billing_charged_count,
    EXCLUDED.billing_unknown_count
);
    RETURN NEW;
END $$;

-- 缺口标记不能随统计写故障回滚；运行期覆盖由观察写入事务显式刷新。
-- 只回填仍保留明细的运行；过期或缺失明细的运行保持未知。
UPDATE stats_usage_checkpoints AS c
SET recovered_request_count = recovery.recovered_request_count,
    recovery_known_request_count = recovery.recovery_known_request_count
FROM stats_usage_run_recovery_source AS recovery
WHERE c.source_kind = 'run'
  AND c.scope = 'global'
  AND recovery.run_id = c.source_id
  AND EXISTS (
    SELECT 1 FROM inference_run_observations AS r
    WHERE r.id = c.source_id
      AND r.expires_at > (EXTRACT(EPOCH FROM now()) * 1000)::bigint
  );

-- 新列从零开始，回填只增加保留期内可证明的贡献。
WITH recovery AS (
    SELECT scope, identity, bucket_start,
           SUM(recovered_request_count) AS recovered_request_count,
           SUM(recovery_known_request_count) AS recovery_known_request_count
    FROM stats_usage_checkpoints
    WHERE source_kind = 'run' AND scope = 'global'
    GROUP BY scope, identity, bucket_start
)
UPDATE stats_usage_buckets AS b
SET recovered_request_count = b.recovered_request_count + recovery.recovered_request_count,
    recovery_known_request_count = b.recovery_known_request_count + recovery.recovery_known_request_count
FROM recovery
WHERE b.scope = recovery.scope
  AND b.identity = recovery.identity
  AND b.bucket_start = recovery.bucket_start;

-- checkpoint 行被改写时，把 NEW − OLD 的差额结转进长期桶；回填之后才创建触发器，避免回填被重复计入。
CREATE FUNCTION stats_usage_delta() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
INSERT INTO stats_usage_buckets (
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    terminal_request_count,
    failed_request_count,
    recovered_request_count,
    recovery_known_request_count,
    upstream_request_count,
    upstream_error_count,
    input_tokens_sum,
    input_tokens_known,
    output_tokens_sum,
    output_tokens_known,
    cache_read_tokens_sum,
    cache_read_tokens_known,
    cache_write_tokens_sum,
    cache_write_tokens_known,
    reasoning_tokens_sum,
    reasoning_tokens_known,
    duration_ms_sum,
    duration_ms_count,
    first_token_ms_sum,
    first_token_ms_count,
    successful_attempt_count,
    successful_output_known_count,
    successful_duration_known_count,
    successful_output_sum,
    successful_duration_sum,
    last_used_at,
    cost_custom_nanos,
    cost_usd_unconverted_nanos,
    cost_known_count,
    cost_unknown_count,
    payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos,
    payg_cost_known_count,
    payg_cost_unknown_count,
    billing_custom_nanos,
    billing_usd_unconverted_nanos,
    billing_charged_count,
    billing_unknown_count
) VALUES (
    NEW.scope,
    NEW.identity,
    NEW.bucket_start,
    NEW.entity_id,
    NEW.label,
    NEW.request_count,
    NEW.terminal_request_count,
    NEW.failed_request_count,
    NEW.recovered_request_count,
    NEW.recovery_known_request_count,
    NEW.upstream_request_count,
    NEW.upstream_error_count,
    NEW.input_tokens_sum,
    NEW.input_tokens_known,
    NEW.output_tokens_sum,
    NEW.output_tokens_known,
    NEW.cache_read_tokens_sum,
    NEW.cache_read_tokens_known,
    NEW.cache_write_tokens_sum,
    NEW.cache_write_tokens_known,
    NEW.reasoning_tokens_sum,
    NEW.reasoning_tokens_known,
    NEW.duration_ms_sum,
    NEW.duration_ms_count,
    NEW.first_token_ms_sum,
    NEW.first_token_ms_count,
    NEW.successful_attempt_count,
    NEW.successful_output_known_count,
    NEW.successful_duration_known_count,
    NEW.successful_output_sum,
    NEW.successful_duration_sum,
    NEW.last_used_at,
    NEW.cost_custom_nanos,
    NEW.cost_usd_unconverted_nanos,
    NEW.cost_known_count,
    NEW.cost_unknown_count,
    NEW.payg_cost_custom_nanos,
    NEW.payg_cost_usd_unconverted_nanos,
    NEW.payg_cost_known_count,
    NEW.payg_cost_unknown_count,
    NEW.billing_custom_nanos,
    NEW.billing_usd_unconverted_nanos,
    NEW.billing_charged_count,
    NEW.billing_unknown_count
) ON CONFLICT (scope, identity, bucket_start) DO UPDATE SET
    request_count = stats_usage_buckets.request_count + excluded.request_count - OLD.request_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count + excluded.terminal_request_count - OLD.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count + excluded.failed_request_count - OLD.failed_request_count,
    recovered_request_count = stats_usage_buckets.recovered_request_count + excluded.recovered_request_count - OLD.recovered_request_count,
    recovery_known_request_count = stats_usage_buckets.recovery_known_request_count + excluded.recovery_known_request_count - OLD.recovery_known_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count + excluded.upstream_request_count - OLD.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count + excluded.upstream_error_count - OLD.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum + excluded.input_tokens_sum - OLD.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known + excluded.input_tokens_known - OLD.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum + excluded.output_tokens_sum - OLD.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known + excluded.output_tokens_known - OLD.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum + excluded.cache_read_tokens_sum - OLD.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known + excluded.cache_read_tokens_known - OLD.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum + excluded.cache_write_tokens_sum - OLD.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known + excluded.cache_write_tokens_known - OLD.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum + excluded.reasoning_tokens_sum - OLD.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known + excluded.reasoning_tokens_known - OLD.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum + excluded.duration_ms_sum - OLD.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count + excluded.duration_ms_count - OLD.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum + excluded.first_token_ms_sum - OLD.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count + excluded.first_token_ms_count - OLD.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count + excluded.successful_attempt_count - OLD.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count + excluded.successful_output_known_count - OLD.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count + excluded.successful_duration_known_count - OLD.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum + excluded.successful_output_sum - OLD.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum + excluded.successful_duration_sum - OLD.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos + excluded.cost_custom_nanos - OLD.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos + excluded.cost_usd_unconverted_nanos - OLD.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count + excluded.cost_known_count - OLD.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count + excluded.cost_unknown_count - OLD.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos + excluded.payg_cost_custom_nanos - OLD.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos + excluded.payg_cost_usd_unconverted_nanos - OLD.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count + excluded.payg_cost_known_count - OLD.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count + excluded.payg_cost_unknown_count - OLD.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos + excluded.billing_custom_nanos - OLD.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos + excluded.billing_usd_unconverted_nanos - OLD.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count + excluded.billing_charged_count - OLD.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count + excluded.billing_unknown_count - OLD.billing_unknown_count,
    label = excluded.label,
    last_used_at = GREATEST(stats_usage_buckets.last_used_at, excluded.last_used_at);
    RETURN NEW;
END $$;
CREATE TRIGGER stats_usage_delta AFTER UPDATE ON stats_usage_checkpoints
FOR EACH ROW EXECUTE FUNCTION stats_usage_delta();

-- 来源行仍在而 checkpoint 被删，说明来源不再产出该行，需从长期桶扣除；
-- 来源行已被保留期清理删除时 reclaim 才会删 checkpoint，历史总量必须保留，故不扣。
CREATE FUNCTION stats_usage_debit() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    source_present boolean;
BEGIN
    SELECT CASE OLD.source_kind
        WHEN 'attempt' THEN EXISTS (SELECT 1 FROM target_attempt_observations WHERE id = OLD.source_id)
        WHEN 'turn' THEN EXISTS (SELECT 1 FROM model_turn_observations WHERE id = OLD.source_id)
        WHEN 'run' THEN EXISTS (SELECT 1 FROM inference_run_observations WHERE id = OLD.source_id)
        ELSE EXISTS (SELECT 1 FROM rejected_request_observations WHERE id = OLD.source_id)
    END INTO source_present;
    IF source_present THEN
        UPDATE stats_usage_buckets SET
            request_count = request_count - OLD.request_count,
            terminal_request_count = terminal_request_count - OLD.terminal_request_count,
            failed_request_count = failed_request_count - OLD.failed_request_count,
            recovered_request_count = recovered_request_count - OLD.recovered_request_count,
            recovery_known_request_count = recovery_known_request_count - OLD.recovery_known_request_count,
            upstream_request_count = upstream_request_count - OLD.upstream_request_count,
            upstream_error_count = upstream_error_count - OLD.upstream_error_count,
            input_tokens_sum = input_tokens_sum - OLD.input_tokens_sum,
            input_tokens_known = input_tokens_known - OLD.input_tokens_known,
            output_tokens_sum = output_tokens_sum - OLD.output_tokens_sum,
            output_tokens_known = output_tokens_known - OLD.output_tokens_known,
            cache_read_tokens_sum = cache_read_tokens_sum - OLD.cache_read_tokens_sum,
            cache_read_tokens_known = cache_read_tokens_known - OLD.cache_read_tokens_known,
            cache_write_tokens_sum = cache_write_tokens_sum - OLD.cache_write_tokens_sum,
            cache_write_tokens_known = cache_write_tokens_known - OLD.cache_write_tokens_known,
            reasoning_tokens_sum = reasoning_tokens_sum - OLD.reasoning_tokens_sum,
            reasoning_tokens_known = reasoning_tokens_known - OLD.reasoning_tokens_known,
            duration_ms_sum = duration_ms_sum - OLD.duration_ms_sum,
            duration_ms_count = duration_ms_count - OLD.duration_ms_count,
            first_token_ms_sum = first_token_ms_sum - OLD.first_token_ms_sum,
            first_token_ms_count = first_token_ms_count - OLD.first_token_ms_count,
            successful_attempt_count = successful_attempt_count - OLD.successful_attempt_count,
            successful_output_known_count = successful_output_known_count - OLD.successful_output_known_count,
            successful_duration_known_count = successful_duration_known_count - OLD.successful_duration_known_count,
            successful_output_sum = successful_output_sum - OLD.successful_output_sum,
            successful_duration_sum = successful_duration_sum - OLD.successful_duration_sum,
            cost_custom_nanos = cost_custom_nanos - OLD.cost_custom_nanos,
            cost_usd_unconverted_nanos = cost_usd_unconverted_nanos - OLD.cost_usd_unconverted_nanos,
            cost_known_count = cost_known_count - OLD.cost_known_count,
            cost_unknown_count = cost_unknown_count - OLD.cost_unknown_count,
            payg_cost_custom_nanos = payg_cost_custom_nanos - OLD.payg_cost_custom_nanos,
            payg_cost_usd_unconverted_nanos = payg_cost_usd_unconverted_nanos - OLD.payg_cost_usd_unconverted_nanos,
            payg_cost_known_count = payg_cost_known_count - OLD.payg_cost_known_count,
            payg_cost_unknown_count = payg_cost_unknown_count - OLD.payg_cost_unknown_count,
            billing_custom_nanos = billing_custom_nanos - OLD.billing_custom_nanos,
            billing_usd_unconverted_nanos = billing_usd_unconverted_nanos - OLD.billing_usd_unconverted_nanos,
            billing_charged_count = billing_charged_count - OLD.billing_charged_count,
            billing_unknown_count = billing_unknown_count - OLD.billing_unknown_count
        WHERE scope = OLD.scope AND identity = OLD.identity AND bucket_start = OLD.bucket_start;
        DELETE FROM stats_usage_buckets
        WHERE scope = OLD.scope AND identity = OLD.identity AND bucket_start = OLD.bucket_start
            AND request_count = 0 AND terminal_request_count = 0 AND failed_request_count = 0
            AND recovered_request_count = 0 AND recovery_known_request_count = 0
            AND upstream_request_count = 0 AND upstream_error_count = 0 AND input_tokens_sum = 0
            AND input_tokens_known = 0 AND output_tokens_sum = 0 AND output_tokens_known = 0
            AND cache_read_tokens_sum = 0 AND cache_read_tokens_known = 0
            AND cache_write_tokens_sum = 0 AND cache_write_tokens_known = 0
            AND reasoning_tokens_sum = 0 AND reasoning_tokens_known = 0 AND duration_ms_sum = 0
            AND duration_ms_count = 0 AND first_token_ms_sum = 0 AND first_token_ms_count = 0
            AND successful_attempt_count = 0 AND successful_output_known_count = 0
            AND successful_duration_known_count = 0 AND successful_output_sum = 0
            AND successful_duration_sum = 0 AND cost_custom_nanos = 0
            AND cost_usd_unconverted_nanos = 0 AND cost_known_count = 0 AND cost_unknown_count = 0
            AND payg_cost_custom_nanos = 0 AND payg_cost_usd_unconverted_nanos = 0
            AND payg_cost_known_count = 0 AND payg_cost_unknown_count = 0
            AND billing_custom_nanos = 0 AND billing_usd_unconverted_nanos = 0
            AND billing_charged_count = 0 AND billing_unknown_count = 0;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER stats_usage_debit AFTER DELETE ON stats_usage_checkpoints
FOR EACH ROW EXECUTE FUNCTION stats_usage_debit();
