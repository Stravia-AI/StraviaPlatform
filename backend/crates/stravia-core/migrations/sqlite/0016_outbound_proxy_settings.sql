-- Preserve the effective model egress before removing the global gate.
-- Trim the complete Unicode White_Space set, matching the old Rust str::trim parser.
UPDATE providers SET use_proxy = CASE WHEN use_proxy <> 0 AND COALESCE(
    (SELECT lower(trim(value, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))) IN ('1', 'true', 'yes', 'on') FROM settings WHERE name = 'proxy_enabled'), 0
) THEN 1 ELSE 0 END
WHERE EXISTS (SELECT 1 FROM settings WHERE name IN ('proxy_enabled', 'proxy_url', 'proxy_bypass', 'proxy_force_http1'))
   OR NOT EXISTS (SELECT 1 FROM settings WHERE name = 'outbound_proxy');

INSERT INTO settings(name, value)
SELECT 'outbound_proxy', json_object(
    'url', COALESCE((SELECT value FROM settings WHERE name = 'proxy_url'), ''),
    'bypass', COALESCE((SELECT value FROM settings WHERE name = 'proxy_bypass'), ''),
    'force_http1', json(CASE WHEN COALESCE((SELECT lower(trim(value, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))) IN ('1', 'true', 'yes', 'on') FROM settings WHERE name = 'proxy_force_http1'), 0) THEN 'true' ELSE 'false' END)
)
WHERE true
ON CONFLICT(name) DO NOTHING;

INSERT INTO settings(name, value)
SELECT 'update_use_proxy', CASE WHEN COALESCE((SELECT lower(trim(value, char(9, 10, 11, 12, 13, 32, 133, 160, 5760, 8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202, 8232, 8233, 8239, 8287, 12288))) IN ('1', 'true', 'yes', 'on') FROM settings WHERE name = 'proxy_enabled'), 0) THEN 'true' ELSE 'false' END
WHERE true
ON CONFLICT(name) DO NOTHING;

DELETE FROM settings WHERE name IN ('proxy_enabled', 'proxy_url', 'proxy_bypass', 'proxy_force_http1');
