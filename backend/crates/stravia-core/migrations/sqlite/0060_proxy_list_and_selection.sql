-- 代理设置升级为命名代理列表，移除绕过地址；供应商与更新代理偏好改为 proxy_mode（none / system / <proxy_id>）。

DROP TRIGGER IF EXISTS providers_contract_insert;
DROP TRIGGER IF EXISTS providers_contract_update;
DROP TRIGGER IF EXISTS web_providers_boolean_insert;
DROP TRIGGER IF EXISTS web_providers_boolean_update;

-- 1. 迁移 outbound_proxy 设置为列表结构
UPDATE settings
SET value = CASE
    WHEN json_valid(value)
     AND json_type(value) = 'object'
     AND json_type(value, '$.proxies') = 'array'
    THEN value
    WHEN json_valid(value)
     AND json_type(value) = 'object'
     AND length(trim(COALESCE(json_extract(value, '$.url'), ''))) > 0
    THEN json_object(
        'proxies',
        json_array(
            json_object(
                'id', 'default',
                'name', 'Default',
                'url', trim(json_extract(value, '$.url'))
            )
        )
    )
    ELSE '{"proxies":[]}'
END
WHERE name = 'outbound_proxy';

-- 2. 迁移 providers.use_proxy -> providers.proxy_mode（存量关闭保留 none，新建默认 system）
ALTER TABLE providers
    ADD COLUMN proxy_mode TEXT NOT NULL DEFAULT 'system' CHECK (length(trim(proxy_mode)) > 0);

UPDATE providers
SET proxy_mode = CASE
    WHEN use_proxy <> 0 THEN
        CASE
            WHEN EXISTS (
                SELECT 1 FROM settings
                WHERE name = 'outbound_proxy'
                  AND json_valid(value)
                  AND json_array_length(value, '$.proxies') > 0
            ) THEN 'default'
            ELSE 'system'
        END
    ELSE 'none'
END;

ALTER TABLE providers DROP COLUMN use_proxy;

CREATE TRIGGER providers_contract_insert BEFORE INSERT ON providers
WHEN CASE WHEN json_valid(NEW.adapter_credentials) THEN json_type(NEW.adapter_credentials) <> 'object' ELSE 1 END
    OR CASE WHEN json_valid(NEW.vendor_options) THEN json_type(NEW.vendor_options) <> 'object' ELSE 1 END
    OR typeof(NEW.proxy_mode) <> 'text' OR length(trim(NEW.proxy_mode)) = 0
    OR typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)
BEGIN SELECT RAISE(ABORT, 'invalid Provider configuration'); END;

CREATE TRIGGER providers_contract_update BEFORE UPDATE ON providers
WHEN CASE WHEN json_valid(NEW.adapter_credentials) THEN json_type(NEW.adapter_credentials) <> 'object' ELSE 1 END
    OR CASE WHEN json_valid(NEW.vendor_options) THEN json_type(NEW.vendor_options) <> 'object' ELSE 1 END
    OR typeof(NEW.proxy_mode) <> 'text' OR length(trim(NEW.proxy_mode)) = 0
    OR typeof(NEW.is_enabled) <> 'integer' OR NEW.is_enabled NOT IN (0, 1)
BEGIN SELECT RAISE(ABORT, 'invalid Provider configuration'); END;

-- 3. 迁移 web_providers.use_proxy -> web_providers.proxy_mode
ALTER TABLE web_providers
    ADD COLUMN proxy_mode TEXT NOT NULL DEFAULT 'system' CHECK (length(trim(proxy_mode)) > 0);

UPDATE web_providers
SET proxy_mode = CASE
    WHEN use_proxy <> 0 THEN
        CASE
            WHEN EXISTS (
                SELECT 1 FROM settings
                WHERE name = 'outbound_proxy'
                  AND json_valid(value)
                  AND json_array_length(value, '$.proxies') > 0
            ) THEN 'default'
            ELSE 'system'
        END
    ELSE 'none'
END;

ALTER TABLE web_providers DROP COLUMN use_proxy;

CREATE TRIGGER web_providers_boolean_insert BEFORE INSERT ON web_providers
WHEN (typeof(NEW.proxy_mode) <> 'text' OR length(trim(NEW.proxy_mode)) = 0)
  OR (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
  OR (NEW.config_json IS NOT NULL AND CASE WHEN json_valid(NEW.config_json) THEN json_type(NEW.config_json) <> 'object' ELSE 1 END)
BEGIN SELECT RAISE(ABORT, 'invalid boolean or provider configuration'); END;

CREATE TRIGGER web_providers_boolean_update BEFORE UPDATE ON web_providers
WHEN (typeof(NEW.proxy_mode) <> 'text' OR length(trim(NEW.proxy_mode)) = 0)
  OR (NEW.last_test_success IS NOT NULL AND (typeof(NEW.last_test_success) <> 'integer' OR NEW.last_test_success NOT IN (0, 1)))
  OR (NEW.config_json IS NOT NULL AND CASE WHEN json_valid(NEW.config_json) THEN json_type(NEW.config_json) <> 'object' ELSE 1 END)
BEGIN SELECT RAISE(ABORT, 'invalid boolean or provider configuration'); END;

-- 4. 迁移 update_use_proxy -> update_proxy_mode
INSERT OR IGNORE INTO settings (name, value)
SELECT
    'update_proxy_mode',
    CASE
        WHEN value = 'true' THEN
            CASE
                WHEN EXISTS (
                    SELECT 1 FROM settings
                    WHERE name = 'outbound_proxy'
                      AND json_valid(value)
                      AND json_array_length(value, '$.proxies') > 0
                ) THEN 'default'
                ELSE 'system'
            END
        ELSE 'none'
    END
FROM settings
WHERE name = 'update_use_proxy';

DELETE FROM settings WHERE name = 'update_use_proxy';
