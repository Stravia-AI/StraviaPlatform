CREATE TABLE vendor_plugins_new (
    vendor_id TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('builtin', 'local', 'marketplace')),
    descriptor TEXT NOT NULL,
    digest TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    data_epoch BIGINT NOT NULL CHECK (data_epoch >= 0),
    installed_at BIGINT NOT NULL,
    origin TEXT
);
INSERT INTO vendor_plugins_new (vendor_id, version, source, descriptor, digest, revision, data_epoch, installed_at)
    SELECT vendor_id, version, source, descriptor, digest, revision, data_epoch, installed_at FROM vendor_plugins;
DROP TABLE vendor_plugins;
ALTER TABLE vendor_plugins_new RENAME TO vendor_plugins;

CREATE TABLE vendor_plugin_check_sequence (id INTEGER PRIMARY KEY, generation BIGINT NOT NULL);
INSERT INTO vendor_plugin_check_sequence (id, generation) VALUES (1, 0);

CREATE TABLE vendor_plugin_sources (
    id TEXT PRIMARY KEY,
    source_json TEXT NOT NULL,
    repository_id TEXT NOT NULL UNIQUE,
    repository TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    trust_revision BIGINT NOT NULL,
    enabled BOOLEAN NOT NULL,
    check_generation BIGINT NOT NULL DEFAULT 0,
    incomplete BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE vendor_plugin_catalogs (
    source_id TEXT NOT NULL REFERENCES vendor_plugin_sources(id) ON DELETE CASCADE,
    release_id TEXT NOT NULL,
    catalog_json TEXT NOT NULL,
    PRIMARY KEY (source_id, release_id)
);

CREATE TABLE vendor_plugin_seen_digests (
    source_id TEXT NOT NULL,
    vendor_id TEXT NOT NULL,
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    conflict BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (source_id, vendor_id, version)
);
