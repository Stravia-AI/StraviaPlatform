-- The built-in Command Code vendor id moved from `commandcode` to
-- `command-code` to match its protocol slug. Rewrite the persisted discovery
-- identities so existing connections keep resolving vendor metadata, catalog
-- scope, and the allowance monitor.
UPDATE providers SET vendor = 'command-code' WHERE vendor = 'commandcode';
UPDATE providers SET preset_key = 'command-code' WHERE preset_key = 'commandcode';
