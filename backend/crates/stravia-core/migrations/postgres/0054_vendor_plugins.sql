CREATE TABLE vendor_plugins (
    vendor_id TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('builtin', 'local')),
    descriptor TEXT NOT NULL,
    digest TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    data_epoch BIGINT NOT NULL CHECK (data_epoch >= 0),
    installed_at BIGINT NOT NULL
);

CREATE TABLE vendor_private_state (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    vendor_id TEXT NOT NULL,
    format_version TEXT NOT NULL,
    payload BYTEA NOT NULL CHECK (octet_length(payload) <= 262144),
    updated_at BIGINT NOT NULL
);

CREATE INDEX idx_vendor_private_state_vendor ON vendor_private_state(vendor_id);

CREATE TABLE vendor_data_recovery (
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    data_kind TEXT NOT NULL CHECK (data_kind IN ('options', 'credentials', 'models')),
    PRIMARY KEY (provider_id, data_kind)
);
