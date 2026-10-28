-- Rebuild stats source views and triggers with cost/billing columns.
-- DDL only: no historical amount backfill; existing checkpoints keep zero deltas.

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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    0 AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    CASE WHEN r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) THEN 1 ELSE 0 END AS error_count,
    CASE WHEN r.status IN ('completed','failed','waiting_client','superseded') AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) AND r.rejected_before_target=0 THEN 1 ELSE 0 END AS terminal_request_count,
    CASE WHEN r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) AND r.rejected_before_target=0 THEN 1 ELSE 0 END AS failed_request_count,
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
    CASE WHEN r.status='failed' AND r.finished_at IS NOT NULL AND (r.failure_json IS NOT NULL OR COALESCE(r.terminal_reason,'') NOT IN ('cancelled','client_disconnected','websocket_delivery_dropped','request_aborted')) THEN 1 ELSE 0 END AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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
    CASE WHEN r.status_code<>499 AND r.code NOT IN ('request_aborted','cancelled','client_disconnected') THEN 1 ELSE 0 END AS error_count,
    0 AS terminal_request_count,
    0 AS failed_request_count,
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

DROP TRIGGER stats_usage_credit;
CREATE TRIGGER stats_usage_credit AFTER INSERT ON stats_usage_checkpoints
BEGIN
INSERT INTO stats_usage_buckets (
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
    NEW.error_count,
    NEW.terminal_request_count,
    NEW.failed_request_count,
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
    error_count = stats_usage_buckets.error_count + excluded.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count + excluded.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count + excluded.failed_request_count,
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
    last_used_at = MAX(stats_usage_buckets.last_used_at, excluded.last_used_at);
END;

DROP TRIGGER stats_usage_turn_insert;
CREATE TRIGGER stats_usage_turn_insert
AFTER INSERT ON model_turn_observations
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'turn' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'turn' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'turn' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'turn' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_turn_source
WHERE source_id = NEW.id;
END;

DROP TRIGGER stats_usage_turn_update;
CREATE TRIGGER stats_usage_turn_update
AFTER UPDATE OF id, route_id, model_display_name, api_key_id, api_key_name, started_at, finished_at ON model_turn_observations
WHEN OLD.id IS NOT NEW.id
    OR OLD.route_id IS NOT NEW.route_id
    OR OLD.model_display_name IS NOT NEW.model_display_name
    OR OLD.api_key_id IS NOT NEW.api_key_id
    OR OLD.api_key_name IS NOT NEW.api_key_name
    OR OLD.started_at IS NOT NEW.started_at
    OR OLD.finished_at IS NOT NEW.finished_at
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'turn' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'turn' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'turn' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'turn' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_turn_source
WHERE source_id = NEW.id;
END;

DROP TRIGGER stats_usage_attempt_insert;
CREATE TRIGGER stats_usage_attempt_insert
AFTER INSERT ON target_attempt_observations
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'attempt' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'attempt' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'attempt' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'attempt' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_attempt_source
WHERE source_id = NEW.id;
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'run' AND c.source_id = NEW.run_id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.run_id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.run_id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'run' AND source_id = NEW.run_id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_run_source
WHERE source_id = NEW.run_id;
END;

DROP TRIGGER stats_usage_attempt_update;
CREATE TRIGGER stats_usage_attempt_update
AFTER UPDATE OF id, model_turn_id, run_id, provider_id, provider_name, status, error_source, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, first_token_ms, duration_ms, cost_basis, cost_usd_nanos, cost_custom_nanos, billing_state, billing_usd_nanos, billing_custom_nanos ON target_attempt_observations
WHEN OLD.id IS NOT NEW.id
    OR OLD.model_turn_id IS NOT NEW.model_turn_id
    OR OLD.run_id IS NOT NEW.run_id
    OR OLD.provider_id IS NOT NEW.provider_id
    OR OLD.provider_name IS NOT NEW.provider_name
    OR OLD.status IS NOT NEW.status
    OR OLD.error_source IS NOT NEW.error_source
    OR OLD.input_tokens IS NOT NEW.input_tokens
    OR OLD.output_tokens IS NOT NEW.output_tokens
    OR OLD.cache_read_tokens IS NOT NEW.cache_read_tokens
    OR OLD.cache_write_tokens IS NOT NEW.cache_write_tokens
    OR OLD.reasoning_tokens IS NOT NEW.reasoning_tokens
    OR OLD.first_token_ms IS NOT NEW.first_token_ms
    OR OLD.duration_ms IS NOT NEW.duration_ms
    OR OLD.cost_basis IS NOT NEW.cost_basis
    OR OLD.cost_usd_nanos IS NOT NEW.cost_usd_nanos
    OR OLD.cost_custom_nanos IS NOT NEW.cost_custom_nanos
    OR OLD.billing_state IS NOT NEW.billing_state
    OR OLD.billing_usd_nanos IS NOT NEW.billing_usd_nanos
    OR OLD.billing_custom_nanos IS NOT NEW.billing_custom_nanos
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'attempt' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'attempt' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'attempt' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'attempt' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_attempt_source
WHERE source_id = NEW.id;
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'run' AND c.source_id = NEW.run_id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.run_id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.run_id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'run' AND source_id = NEW.run_id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_run_source
WHERE source_id = NEW.run_id;
END;

DROP TRIGGER stats_usage_run_insert;
CREATE TRIGGER stats_usage_run_insert
AFTER INSERT ON inference_run_observations
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'run' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'run' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_run_source
WHERE source_id = NEW.id;
END;

DROP TRIGGER stats_usage_run_update;
CREATE TRIGGER stats_usage_run_update
AFTER UPDATE OF id, started_at, status, finished_at, failure_json, terminal_reason, rejected_before_target ON inference_run_observations
WHEN OLD.id IS NOT NEW.id
    OR OLD.started_at IS NOT NEW.started_at
    OR OLD.status IS NOT NEW.status
    OR OLD.finished_at IS NOT NEW.finished_at
    OR OLD.failure_json IS NOT NEW.failure_json
    OR OLD.terminal_reason IS NOT NEW.terminal_reason
    OR OLD.rejected_before_target IS NOT NEW.rejected_before_target
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'run' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'run' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'run' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_run_source
WHERE source_id = NEW.id;
END;

DROP TRIGGER stats_usage_rejection_insert;
CREATE TRIGGER stats_usage_rejection_insert
AFTER INSERT ON rejected_request_observations
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'rejection' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'rejection' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'rejection' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'rejection' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_rejection_source
WHERE source_id = NEW.id;
END;

DROP TRIGGER stats_usage_rejection_update;
CREATE TRIGGER stats_usage_rejection_update
AFTER UPDATE OF id, started_at, occurred_at, status_code, code ON rejected_request_observations
WHEN OLD.id IS NOT NEW.id
    OR OLD.started_at IS NOT NEW.started_at
    OR OLD.occurred_at IS NOT NEW.occurred_at
    OR OLD.status_code IS NOT NEW.status_code
    OR OLD.code IS NOT NEW.code
BEGIN
-- Drive the delta from this source checkpoint, never from the growing aggregate history.
UPDATE stats_usage_buckets SET
    request_count = stats_usage_buckets.request_count - c.request_count,
    error_count = stats_usage_buckets.error_count - c.error_count,
    terminal_request_count = stats_usage_buckets.terminal_request_count - c.terminal_request_count,
    failed_request_count = stats_usage_buckets.failed_request_count - c.failed_request_count,
    upstream_request_count = stats_usage_buckets.upstream_request_count - c.upstream_request_count,
    upstream_error_count = stats_usage_buckets.upstream_error_count - c.upstream_error_count,
    input_tokens_sum = stats_usage_buckets.input_tokens_sum - c.input_tokens_sum,
    input_tokens_known = stats_usage_buckets.input_tokens_known - c.input_tokens_known,
    output_tokens_sum = stats_usage_buckets.output_tokens_sum - c.output_tokens_sum,
    output_tokens_known = stats_usage_buckets.output_tokens_known - c.output_tokens_known,
    cache_read_tokens_sum = stats_usage_buckets.cache_read_tokens_sum - c.cache_read_tokens_sum,
    cache_read_tokens_known = stats_usage_buckets.cache_read_tokens_known - c.cache_read_tokens_known,
    cache_write_tokens_sum = stats_usage_buckets.cache_write_tokens_sum - c.cache_write_tokens_sum,
    cache_write_tokens_known = stats_usage_buckets.cache_write_tokens_known - c.cache_write_tokens_known,
    reasoning_tokens_sum = stats_usage_buckets.reasoning_tokens_sum - c.reasoning_tokens_sum,
    reasoning_tokens_known = stats_usage_buckets.reasoning_tokens_known - c.reasoning_tokens_known,
    duration_ms_sum = stats_usage_buckets.duration_ms_sum - c.duration_ms_sum,
    duration_ms_count = stats_usage_buckets.duration_ms_count - c.duration_ms_count,
    first_token_ms_sum = stats_usage_buckets.first_token_ms_sum - c.first_token_ms_sum,
    first_token_ms_count = stats_usage_buckets.first_token_ms_count - c.first_token_ms_count,
    successful_attempt_count = stats_usage_buckets.successful_attempt_count - c.successful_attempt_count,
    successful_output_known_count = stats_usage_buckets.successful_output_known_count - c.successful_output_known_count,
    successful_duration_known_count = stats_usage_buckets.successful_duration_known_count - c.successful_duration_known_count,
    successful_output_sum = stats_usage_buckets.successful_output_sum - c.successful_output_sum,
    successful_duration_sum = stats_usage_buckets.successful_duration_sum - c.successful_duration_sum,
    cost_custom_nanos = stats_usage_buckets.cost_custom_nanos - c.cost_custom_nanos,
    cost_usd_unconverted_nanos = stats_usage_buckets.cost_usd_unconverted_nanos - c.cost_usd_unconverted_nanos,
    cost_known_count = stats_usage_buckets.cost_known_count - c.cost_known_count,
    cost_unknown_count = stats_usage_buckets.cost_unknown_count - c.cost_unknown_count,
    payg_cost_custom_nanos = stats_usage_buckets.payg_cost_custom_nanos - c.payg_cost_custom_nanos,
    payg_cost_usd_unconverted_nanos = stats_usage_buckets.payg_cost_usd_unconverted_nanos - c.payg_cost_usd_unconverted_nanos,
    payg_cost_known_count = stats_usage_buckets.payg_cost_known_count - c.payg_cost_known_count,
    payg_cost_unknown_count = stats_usage_buckets.payg_cost_unknown_count - c.payg_cost_unknown_count,
    billing_custom_nanos = stats_usage_buckets.billing_custom_nanos - c.billing_custom_nanos,
    billing_usd_unconverted_nanos = stats_usage_buckets.billing_usd_unconverted_nanos - c.billing_usd_unconverted_nanos,
    billing_charged_count = stats_usage_buckets.billing_charged_count - c.billing_charged_count,
    billing_unknown_count = stats_usage_buckets.billing_unknown_count - c.billing_unknown_count
FROM stats_usage_checkpoints AS c
WHERE c.source_kind = 'rejection' AND c.source_id = NEW.id
  AND c.scope = stats_usage_buckets.scope
  AND c.identity = stats_usage_buckets.identity
  AND c.bucket_start = stats_usage_buckets.bucket_start
  AND (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'rejection' AND c.source_id = NEW.id
);
DELETE FROM stats_usage_buckets
WHERE (stats_usage_buckets.scope, stats_usage_buckets.identity, stats_usage_buckets.bucket_start) IN (
    SELECT c.scope, c.identity, c.bucket_start
    FROM stats_usage_checkpoints AS c
    WHERE c.source_kind = 'rejection' AND c.source_id = NEW.id
)
  AND stats_usage_buckets.request_count = 0
  AND stats_usage_buckets.error_count = 0
  AND stats_usage_buckets.terminal_request_count = 0
  AND stats_usage_buckets.failed_request_count = 0
  AND stats_usage_buckets.upstream_request_count = 0
  AND stats_usage_buckets.upstream_error_count = 0
  AND stats_usage_buckets.input_tokens_sum = 0
  AND stats_usage_buckets.input_tokens_known = 0
  AND stats_usage_buckets.output_tokens_sum = 0
  AND stats_usage_buckets.output_tokens_known = 0
  AND stats_usage_buckets.cache_read_tokens_sum = 0
  AND stats_usage_buckets.cache_read_tokens_known = 0
  AND stats_usage_buckets.cache_write_tokens_sum = 0
  AND stats_usage_buckets.cache_write_tokens_known = 0
  AND stats_usage_buckets.reasoning_tokens_sum = 0
  AND stats_usage_buckets.reasoning_tokens_known = 0
  AND stats_usage_buckets.duration_ms_sum = 0
  AND stats_usage_buckets.duration_ms_count = 0
  AND stats_usage_buckets.first_token_ms_sum = 0
  AND stats_usage_buckets.first_token_ms_count = 0
  AND stats_usage_buckets.successful_attempt_count = 0
  AND stats_usage_buckets.successful_output_known_count = 0
  AND stats_usage_buckets.successful_duration_known_count = 0
  AND stats_usage_buckets.successful_output_sum = 0
  AND stats_usage_buckets.successful_duration_sum = 0
  AND stats_usage_buckets.cost_custom_nanos = 0
  AND stats_usage_buckets.cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.cost_known_count = 0
  AND stats_usage_buckets.cost_unknown_count = 0
  AND stats_usage_buckets.payg_cost_custom_nanos = 0
  AND stats_usage_buckets.payg_cost_usd_unconverted_nanos = 0
  AND stats_usage_buckets.payg_cost_known_count = 0
  AND stats_usage_buckets.payg_cost_unknown_count = 0
  AND stats_usage_buckets.billing_custom_nanos = 0
  AND stats_usage_buckets.billing_usd_unconverted_nanos = 0
  AND stats_usage_buckets.billing_charged_count = 0
  AND stats_usage_buckets.billing_unknown_count = 0;
DELETE FROM stats_usage_checkpoints
WHERE source_kind = 'rejection' AND source_id = NEW.id;
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
    terminal_request_count,
    failed_request_count,
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
) SELECT
    source_kind,
    source_id,
    scope,
    identity,
    bucket_start,
    entity_id,
    label,
    request_count,
    error_count,
    terminal_request_count,
    failed_request_count,
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
FROM stats_usage_rejection_source
WHERE source_id = NEW.id;
END;

-- 保留期内的既有运行按固定错误码回填“错误的请求”；请求 Hook 拒绝无法从旧数据恢复。
-- 已记录 Target 尝试的运行说明进入了选择阶段，错误码只是最终失败原因，不能算准入前拒绝。

-- Cumulative spend only: the API Key row is skipped when the key was deleted.
CREATE TRIGGER api_key_spend_credit_insert AFTER INSERT ON target_attempt_observations
WHEN NEW.billing_state = 'charged'
BEGIN
    INSERT INTO api_key_spend (
        api_key_id,
        spent_custom_nanos,
        spent_usd_unconverted_nanos,
        reset_at,
        updated_at
    )
    SELECT
        t.api_key_id,
        CASE WHEN NEW.billing_custom_nanos IS NOT NULL THEN NEW.billing_custom_nanos ELSE 0 END,
        CASE WHEN NEW.billing_custom_nanos IS NULL THEN COALESCE(NEW.billing_usd_nanos, 0) ELSE 0 END,
        NULL,
        strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
    FROM model_turn_observations t
    WHERE t.id = NEW.model_turn_id
      AND t.api_key_id IS NOT NULL
      AND EXISTS (SELECT 1 FROM api_keys k WHERE k.id = t.api_key_id)
    ON CONFLICT(api_key_id) DO UPDATE SET
        spent_custom_nanos = api_key_spend.spent_custom_nanos + excluded.spent_custom_nanos,
        spent_usd_unconverted_nanos = api_key_spend.spent_usd_unconverted_nanos + excluded.spent_usd_unconverted_nanos,
        updated_at = excluded.updated_at;
END;

CREATE TRIGGER api_key_spend_credit_update AFTER UPDATE OF billing_state ON target_attempt_observations
WHEN NEW.billing_state = 'charged' AND OLD.billing_state IS NOT 'charged'
BEGIN
    INSERT INTO api_key_spend (
        api_key_id,
        spent_custom_nanos,
        spent_usd_unconverted_nanos,
        reset_at,
        updated_at
    )
    SELECT
        t.api_key_id,
        CASE WHEN NEW.billing_custom_nanos IS NOT NULL THEN NEW.billing_custom_nanos ELSE 0 END,
        CASE WHEN NEW.billing_custom_nanos IS NULL THEN COALESCE(NEW.billing_usd_nanos, 0) ELSE 0 END,
        NULL,
        strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
    FROM model_turn_observations t
    WHERE t.id = NEW.model_turn_id
      AND t.api_key_id IS NOT NULL
      AND EXISTS (SELECT 1 FROM api_keys k WHERE k.id = t.api_key_id)
    ON CONFLICT(api_key_id) DO UPDATE SET
        spent_custom_nanos = api_key_spend.spent_custom_nanos + excluded.spent_custom_nanos,
        spent_usd_unconverted_nanos = api_key_spend.spent_usd_unconverted_nanos + excluded.spent_usd_unconverted_nanos,
        updated_at = excluded.updated_at;
END;
