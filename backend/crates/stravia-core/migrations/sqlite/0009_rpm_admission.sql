-- 旧并发上限与 RPM 单位不等价，迁移后由管理员重新配置。
ALTER TABLE api_keys ADD COLUMN rpm_limit INTEGER CHECK (rpm_limit IS NULL OR rpm_limit > 0);
ALTER TABLE api_keys DROP COLUMN concurrency_limit;
ALTER TABLE model_backends ADD COLUMN rpm_pool_id TEXT;
