-- 为 model_backends 增加轻量配置归属标记 config_mode（shared / custom），
-- 同时保留 4 项运行参数与思考等级映射在每条 Target 行上的物化存储，推理选路热路径零改动。
-- 升级时按 (model_id, enabled, priority, model) 同层同模型组识别存量分歧：
-- 组内配置完全一致则保持 shared；若有分歧且存在多数配置（出现次数 > 1），多数保持 shared、其余标为 custom；
-- 若分歧组不存在多数（例如 1v1 两项各不相同），整组标为 custom 以避免静默覆盖手工配置。

DROP TRIGGER IF EXISTS model_backends_contract_insert;
DROP TRIGGER IF EXISTS model_backends_contract_update;

ALTER TABLE model_backends
    ADD COLUMN config_mode TEXT NOT NULL DEFAULT 'shared'
    CHECK (config_mode IN ('shared', 'custom'));

WITH group_variants AS (
    SELECT
        model_id,
        enabled,
        priority,
        model,
        first_token_timeout_ms,
        target_retry_budget,
        target_cooldown_ms,
        thinking_level_map,
        COUNT(*) AS variant_count,
        MIN(created_at) AS first_created_at,
        MIN(id) AS first_id
    FROM model_backends
    GROUP BY
        model_id,
        enabled,
        priority,
        model,
        first_token_timeout_ms,
        target_retry_budget,
        target_cooldown_ms,
        thinking_level_map
),
diverged_groups AS (
    SELECT model_id, enabled, priority, model
    FROM group_variants
    GROUP BY model_id, enabled, priority, model
    HAVING COUNT(*) > 1
),
majority_variant AS (
    SELECT
        gv.model_id,
        gv.enabled,
        gv.priority,
        gv.model,
        gv.first_token_timeout_ms,
        gv.target_retry_budget,
        gv.target_cooldown_ms,
        gv.thinking_level_map,
        ROW_NUMBER() OVER (
            PARTITION BY gv.model_id, gv.enabled, gv.priority, gv.model
            ORDER BY gv.variant_count DESC, gv.first_created_at ASC, gv.first_id ASC
        ) AS rn
    FROM group_variants gv
    JOIN diverged_groups dg
      ON dg.model_id = gv.model_id
     AND dg.enabled = gv.enabled
     AND dg.priority = gv.priority
     AND dg.model = gv.model
    WHERE gv.variant_count > 1
)
UPDATE model_backends
SET config_mode = 'custom'
WHERE EXISTS (
    SELECT 1
    FROM diverged_groups dg
    WHERE dg.model_id = model_backends.model_id
      AND dg.enabled = model_backends.enabled
      AND dg.priority = model_backends.priority
      AND dg.model = model_backends.model
)
  AND NOT EXISTS (
    SELECT 1
    FROM majority_variant mv
    WHERE mv.rn = 1
      AND mv.model_id = model_backends.model_id
      AND mv.enabled = model_backends.enabled
      AND mv.priority = model_backends.priority
      AND mv.model = model_backends.model
      AND mv.first_token_timeout_ms = model_backends.first_token_timeout_ms
      AND mv.target_retry_budget = model_backends.target_retry_budget
      AND mv.target_cooldown_ms = model_backends.target_cooldown_ms
      AND mv.thinking_level_map = model_backends.thinking_level_map
);

CREATE TRIGGER model_backends_contract_insert BEFORE INSERT ON model_backends
WHEN NEW.priority IS NULL OR typeof(NEW.priority) <> 'integer' OR NEW.priority < -2147483648 OR NEW.priority > 2147483647
    OR typeof(NEW.target_retry_budget) <> 'integer' OR NEW.target_retry_budget < 0 OR NEW.target_retry_budget > 2147483647
    OR typeof(NEW.first_token_timeout_ms) <> 'integer' OR NEW.first_token_timeout_ms < 0
    OR typeof(NEW.target_cooldown_ms) <> 'integer' OR NEW.target_cooldown_ms < 0
    OR typeof(NEW.enabled) <> 'integer' OR NEW.enabled NOT IN (0, 1)
    OR typeof(NEW.config_mode) <> 'text' OR NEW.config_mode NOT IN ('shared', 'custom')
    OR CASE WHEN json_valid(NEW.thinking_level_map) THEN json_type(NEW.thinking_level_map) <> 'array' ELSE 1 END
BEGIN SELECT RAISE(ABORT, 'invalid Target configuration'); END;

CREATE TRIGGER model_backends_contract_update BEFORE UPDATE ON model_backends
WHEN NEW.priority IS NULL OR typeof(NEW.priority) <> 'integer' OR NEW.priority < -2147483648 OR NEW.priority > 2147483647
    OR typeof(NEW.target_retry_budget) <> 'integer' OR NEW.target_retry_budget < 0 OR NEW.target_retry_budget > 2147483647
    OR typeof(NEW.first_token_timeout_ms) <> 'integer' OR NEW.first_token_timeout_ms < 0
    OR typeof(NEW.target_cooldown_ms) <> 'integer' OR NEW.target_cooldown_ms < 0
    OR typeof(NEW.enabled) <> 'integer' OR NEW.enabled NOT IN (0, 1)
    OR typeof(NEW.config_mode) <> 'text' OR NEW.config_mode NOT IN ('shared', 'custom')
    OR CASE WHEN json_valid(NEW.thinking_level_map) THEN json_type(NEW.thinking_level_map) <> 'array' ELSE 1 END
BEGIN SELECT RAISE(ABORT, 'invalid Target configuration'); END;
