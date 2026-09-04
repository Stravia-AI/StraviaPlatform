ALTER TABLE model_backends
ADD COLUMN first_token_timeout_ms BIGINT NOT NULL DEFAULT 60000;

ALTER TABLE model_backends
ADD COLUMN target_retry_budget INTEGER NOT NULL DEFAULT 5;

ALTER TABLE model_backends
ADD COLUMN target_cooldown_ms BIGINT NOT NULL DEFAULT 120000;

UPDATE model_backends SET priority = 0;

UPDATE models
SET balance = CASE
    WHEN lower(trim(COALESCE(balance, ''))) = 'latency' THEN 'latency_preference'
    ELSE 'traffic_equalization'
END;

ALTER TABLE models ALTER COLUMN balance SET DEFAULT 'traffic_equalization';
ALTER TABLE model_backends ALTER COLUMN priority SET DEFAULT 0;
ALTER TABLE model_backends DROP COLUMN weight;
