-- The four generic `protocol-*` provider profiles are merged into the
-- base-owned `custom` profile, which selects the egress protocol through its
-- channel options. The stored protocol values already match the Custom
-- channel's selectable set, so only the vendor identity changes. Connection
-- UUIDs, credentials, vendor_options, and Routes are preserved.
UPDATE providers
SET vendor = 'custom'
WHERE vendor IN (
    'protocol-openai-chat-completions',
    'protocol-open-responses',
    'protocol-anthropic-messages',
    'protocol-gemini'
);
