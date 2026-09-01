CREATE TABLE provider_allowance_samples (
    id              TEXT PRIMARY KEY,
    provider_id     TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    allowance_key   TEXT NOT NULL,
    sampled_at      INTEGER NOT NULL,
    used_value      REAL,
    remaining_value REAL,
    limit_value     REAL,
    used_percent    REAL,
    amount_unit     TEXT,
    currency        TEXT,
    reset_at        INTEGER
);

CREATE INDEX idx_provider_allowance_samples_item_time
    ON provider_allowance_samples(provider_id, allowance_key, sampled_at);
CREATE INDEX idx_provider_allowance_samples_sampled_at
    ON provider_allowance_samples(sampled_at);
