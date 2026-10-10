-- Preserve the effective model egress before removing the global gate.
-- Trim the complete Unicode White_Space set, matching the old Rust str::trim parser.
UPDATE providers SET use_proxy = use_proxy AND COALESCE(
    (SELECT lower(btrim(value, E'\u0009\u000A\u000B\u000C\u000D\u0020\u0085\u00A0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200A\u2028\u2029\u202F\u205F\u3000')) IN ('1', 'true', 'yes', 'on') FROM settings WHERE name = 'proxy_enabled'), false
)
WHERE EXISTS (SELECT 1 FROM settings WHERE name IN ('proxy_enabled', 'proxy_url', 'proxy_bypass', 'proxy_force_http1'))
   OR NOT EXISTS (SELECT 1 FROM settings WHERE name = 'outbound_proxy');

INSERT INTO settings(name, value)
SELECT 'outbound_proxy', json_build_object(
    'url', COALESCE((SELECT value FROM settings WHERE name = 'proxy_url'), ''),
    'bypass', COALESCE((SELECT value FROM settings WHERE name = 'proxy_bypass'), ''),
    'force_http1', COALESCE((SELECT lower(btrim(value, E'\u0009\u000A\u000B\u000C\u000D\u0020\u0085\u00A0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200A\u2028\u2029\u202F\u205F\u3000')) IN ('1', 'true', 'yes', 'on') FROM settings WHERE name = 'proxy_force_http1'), false)
)::text
ON CONFLICT(name) DO NOTHING;

INSERT INTO settings(name, value)
SELECT 'update_use_proxy', CASE WHEN COALESCE((SELECT lower(btrim(value, E'\u0009\u000A\u000B\u000C\u000D\u0020\u0085\u00A0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200A\u2028\u2029\u202F\u205F\u3000')) IN ('1', 'true', 'yes', 'on') FROM settings WHERE name = 'proxy_enabled'), false) THEN 'true' ELSE 'false' END
ON CONFLICT(name) DO NOTHING;

DELETE FROM settings WHERE name IN ('proxy_enabled', 'proxy_url', 'proxy_bypass', 'proxy_force_http1');
