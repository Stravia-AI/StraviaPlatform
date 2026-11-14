-- 仅改变新 Target 的首输出空闲窗口默认值，已有配置全部原样保留。
-- SQLite 不支持 ALTER COLUMN SET DEFAULT；重建仅复制 Target 配置，不扫描历史数据。
CREATE TABLE model_backends_new (
    id                     TEXT PRIMARY KEY,
    model_id               TEXT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    provider_id            TEXT NOT NULL REFERENCES providers(id),
    model                  TEXT NOT NULL CHECK (length(trim(model)) > 0),
    priority               INTEGER NOT NULL DEFAULT 0,
    created_at             TEXT NOT NULL DEFAULT (datetime('now')),
    thinking_level_map     TEXT NOT NULL DEFAULT '[{"level":"off","control":{"type":"hidden"},"source":"generated"},{"level":"minimal","control":{"type":"hidden"},"source":"generated"},{"level":"low","control":{"type":"hidden"},"source":"generated"},{"level":"medium","control":{"type":"hidden"},"source":"generated"},{"level":"high","control":{"type":"hidden"},"source":"generated"},{"level":"xhigh","control":{"type":"hidden"},"source":"generated"},{"level":"max","control":{"type":"hidden"},"source":"generated"}]'
        CHECK (json_valid(thinking_level_map)),
    first_token_timeout_ms INTEGER NOT NULL DEFAULT 120000,
    target_retry_budget    INTEGER NOT NULL DEFAULT 5,
    target_cooldown_ms     INTEGER NOT NULL DEFAULT 120000,
    enabled                INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1))
);

INSERT INTO model_backends_new (
    id, model_id, provider_id, model, priority, created_at, thinking_level_map,
    first_token_timeout_ms, target_retry_budget, target_cooldown_ms, enabled
)
SELECT id, model_id, provider_id, model, priority, created_at, thinking_level_map,
       first_token_timeout_ms, target_retry_budget, target_cooldown_ms, enabled
FROM model_backends;
DROP TABLE model_backends;
ALTER TABLE model_backends_new RENAME TO model_backends;
CREATE INDEX idx_model_backends_model_id ON model_backends(model_id);

CREATE TRIGGER model_backends_contract_insert BEFORE INSERT ON model_backends
WHEN NEW.priority IS NULL OR typeof(NEW.priority) <> 'integer' OR NEW.priority < -2147483648 OR NEW.priority > 2147483647
    OR typeof(NEW.target_retry_budget) <> 'integer' OR NEW.target_retry_budget < 0 OR NEW.target_retry_budget > 2147483647
    OR typeof(NEW.first_token_timeout_ms) <> 'integer' OR NEW.first_token_timeout_ms < 0
    OR typeof(NEW.target_cooldown_ms) <> 'integer' OR NEW.target_cooldown_ms < 0
    OR typeof(NEW.enabled) <> 'integer' OR NEW.enabled NOT IN (0, 1)
    OR CASE WHEN json_valid(NEW.thinking_level_map) THEN json_type(NEW.thinking_level_map) <> 'array' ELSE 1 END
BEGIN SELECT RAISE(ABORT, 'invalid Target configuration'); END;
CREATE TRIGGER model_backends_contract_update BEFORE UPDATE ON model_backends
WHEN NEW.priority IS NULL OR typeof(NEW.priority) <> 'integer' OR NEW.priority < -2147483648 OR NEW.priority > 2147483647
    OR typeof(NEW.target_retry_budget) <> 'integer' OR NEW.target_retry_budget < 0 OR NEW.target_retry_budget > 2147483647
    OR typeof(NEW.first_token_timeout_ms) <> 'integer' OR NEW.first_token_timeout_ms < 0
    OR typeof(NEW.target_cooldown_ms) <> 'integer' OR NEW.target_cooldown_ms < 0
    OR typeof(NEW.enabled) <> 'integer' OR NEW.enabled NOT IN (0, 1)
    OR CASE WHEN json_valid(NEW.thinking_level_map) THEN json_type(NEW.thinking_level_map) <> 'array' ELSE 1 END
BEGIN SELECT RAISE(ABORT, 'invalid Target configuration'); END;
