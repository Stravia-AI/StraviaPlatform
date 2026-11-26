-- 代理设置升级为命名代理列表，移除绕过地址；供应商与更新代理偏好改为 proxy_mode（none / system / <proxy_id>）。

-- 1. 迁移 outbound_proxy 设置为列表结构
UPDATE settings
SET value = CASE
    WHEN jsonb_typeof(value::jsonb) = 'object'
     AND jsonb_typeof(value::jsonb -> 'proxies') = 'array'
    THEN value
    WHEN jsonb_typeof(value::jsonb) = 'object'
     AND length(btrim(COALESCE(value::jsonb ->> 'url', ''))) > 0
    THEN jsonb_build_object(
        'proxies',
        jsonb_build_array(
            jsonb_build_object(
                'id', 'default',
                'name', 'Default',
                'url', btrim(value::jsonb ->> 'url')
            )
        )
    )::text
    ELSE '{"proxies":[]}'
END
WHERE name = 'outbound_proxy';

-- 2. 迁移 providers.use_proxy -> providers.proxy_mode（存量关闭保留 none，新建默认 system）
ALTER TABLE providers
    ADD COLUMN proxy_mode text NOT NULL DEFAULT 'system' CHECK (length(btrim(proxy_mode)) > 0);

UPDATE providers
SET proxy_mode = CASE
    WHEN use_proxy THEN
        CASE
            WHEN EXISTS (
                SELECT 1 FROM settings
                WHERE name = 'outbound_proxy'
                  AND jsonb_typeof(value::jsonb -> 'proxies') = 'array'
                  AND jsonb_array_length(value::jsonb -> 'proxies') > 0
            ) THEN 'default'
            ELSE 'system'
        END
    ELSE 'none'
END;

ALTER TABLE providers DROP COLUMN use_proxy;

-- 3. 迁移 web_providers.use_proxy -> web_providers.proxy_mode
ALTER TABLE web_providers
    ADD COLUMN proxy_mode text NOT NULL DEFAULT 'system' CHECK (length(btrim(proxy_mode)) > 0);

UPDATE web_providers
SET proxy_mode = CASE
    WHEN use_proxy THEN
        CASE
            WHEN EXISTS (
                SELECT 1 FROM settings
                WHERE name = 'outbound_proxy'
                  AND jsonb_typeof(value::jsonb -> 'proxies') = 'array'
                  AND jsonb_array_length(value::jsonb -> 'proxies') > 0
            ) THEN 'default'
            ELSE 'system'
        END
    ELSE 'none'
END;

ALTER TABLE web_providers DROP COLUMN use_proxy;

-- 4. 迁移 update_use_proxy -> update_proxy_mode
INSERT INTO settings (name, value)
SELECT
    'update_proxy_mode',
    CASE
        WHEN value = 'true' THEN
            CASE
                WHEN EXISTS (
                    SELECT 1 FROM settings
                    WHERE name = 'outbound_proxy'
                      AND jsonb_typeof(value::jsonb -> 'proxies') = 'array'
                      AND jsonb_array_length(value::jsonb -> 'proxies') > 0
                ) THEN 'default'
                ELSE 'system'
            END
        ELSE 'none'
    END
FROM settings
WHERE name = 'update_use_proxy'
ON CONFLICT (name) DO NOTHING;

DELETE FROM settings WHERE name = 'update_use_proxy';
