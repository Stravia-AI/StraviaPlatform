CREATE TABLE provider_allowance_guards (
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    allowance_key TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (provider_id, allowance_key)
);
CREATE TABLE provider_allowance_suspensions (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    suspended BOOLEAN NOT NULL,
    suspended_at TEXT,
    triggered_keys TEXT NOT NULL,
    earliest_reset_at BIGINT,
    evidence_completed_at BIGINT NOT NULL
);
