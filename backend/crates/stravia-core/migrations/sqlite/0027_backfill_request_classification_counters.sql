-- 为保留期内的明细回填四个新计数；只更新新列，既有 token 与耗时贡献保持不变。
-- 过期明细保持未知（零），因此不会把迁移前的历史伪装成新口径。

UPDATE stats_usage_checkpoints
SET terminal_request_count = v.terminal_request_count,
    failed_request_count = v.failed_request_count,
    upstream_request_count = v.upstream_request_count,
    upstream_error_count = v.upstream_error_count
FROM stats_usage_run_source AS v
WHERE stats_usage_checkpoints.source_kind = 'run'
  AND stats_usage_checkpoints.source_id = v.source_id
  AND stats_usage_checkpoints.scope = v.scope
  AND stats_usage_checkpoints.identity = v.identity
  AND stats_usage_checkpoints.bucket_start = v.bucket_start
  AND EXISTS (
    SELECT 1 FROM inference_run_observations AS r
    WHERE r.id = stats_usage_checkpoints.source_id AND r.expires_at > CAST(strftime('%s','now') AS INTEGER) * 1000
);

UPDATE stats_usage_checkpoints
SET terminal_request_count = v.terminal_request_count,
    failed_request_count = v.failed_request_count,
    upstream_request_count = v.upstream_request_count,
    upstream_error_count = v.upstream_error_count
FROM stats_usage_attempt_source AS v
WHERE stats_usage_checkpoints.source_kind = 'attempt'
  AND stats_usage_checkpoints.source_id = v.source_id
  AND stats_usage_checkpoints.scope = v.scope
  AND stats_usage_checkpoints.identity = v.identity
  AND stats_usage_checkpoints.bucket_start = v.bucket_start
  AND EXISTS (
    SELECT 1 FROM target_attempt_observations AS a
    JOIN inference_run_observations AS r ON r.id = a.run_id
    WHERE a.id = stats_usage_checkpoints.source_id AND r.expires_at > CAST(strftime('%s','now') AS INTEGER) * 1000
);

INSERT INTO stats_usage_checkpoints (
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
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
    terminal_request_count,
    failed_request_count,
    upstream_request_count,
    upstream_error_count,
    last_used_at
)
SELECT
    v.source_kind,
    v.source_id,
    v.scope,
    v.identity,
    v.bucket_start,
    v.entity_id,
    v.label,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    v.terminal_request_count,
    v.failed_request_count,
    v.upstream_request_count,
    v.upstream_error_count,
    v.last_used_at
FROM stats_usage_run_source AS v
WHERE EXISTS (
    SELECT 1 FROM inference_run_observations AS r
    WHERE r.id = v.source_id AND r.expires_at > CAST(strftime('%s','now') AS INTEGER) * 1000
)
  AND NOT EXISTS (
    SELECT 1 FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = v.source_kind AND c.source_id = v.source_id
      AND c.scope = v.scope AND c.identity = v.identity AND c.bucket_start = v.bucket_start
);

INSERT INTO stats_usage_checkpoints (
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
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
    terminal_request_count,
    failed_request_count,
    upstream_request_count,
    upstream_error_count,
    last_used_at
)
SELECT
    v.source_kind,
    v.source_id,
    v.scope,
    v.identity,
    v.bucket_start,
    v.entity_id,
    v.label,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    v.terminal_request_count,
    v.failed_request_count,
    v.upstream_request_count,
    v.upstream_error_count,
    v.last_used_at
FROM stats_usage_attempt_source AS v
WHERE EXISTS (
    SELECT 1 FROM target_attempt_observations AS a
    JOIN inference_run_observations AS r ON r.id = a.run_id
    WHERE a.id = v.source_id AND r.expires_at > CAST(strftime('%s','now') AS INTEGER) * 1000
)
  AND NOT EXISTS (
    SELECT 1 FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = v.source_kind AND c.source_id = v.source_id
      AND c.scope = v.scope AND c.identity = v.identity AND c.bucket_start = v.bucket_start
);

-- 桶的四个新计数等于其检查点之和；旧列不参与重算，避免重复计入。
WITH sums AS (
    SELECT scope, identity, bucket_start,
           SUM(terminal_request_count) AS terminal_request_count,
           SUM(failed_request_count) AS failed_request_count,
           SUM(upstream_request_count) AS upstream_request_count,
           SUM(upstream_error_count) AS upstream_error_count
    FROM stats_usage_checkpoints
    GROUP BY scope, identity, bucket_start
)
UPDATE stats_usage_buckets
SET terminal_request_count = sums.terminal_request_count,
    failed_request_count = sums.failed_request_count,
    upstream_request_count = sums.upstream_request_count,
    upstream_error_count = sums.upstream_error_count
FROM sums
WHERE stats_usage_buckets.scope = sums.scope
  AND stats_usage_buckets.identity = sums.identity
  AND stats_usage_buckets.bucket_start = sums.bucket_start;
