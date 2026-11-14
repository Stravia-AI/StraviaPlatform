-- 仅更新新 Target 的默认值；已有超时、重试预算与冷却配置不变。
ALTER TABLE model_backends
    ALTER COLUMN first_token_timeout_ms SET DEFAULT 120000;
