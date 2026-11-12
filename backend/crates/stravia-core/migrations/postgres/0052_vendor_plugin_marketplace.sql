ALTER TABLE vendor_plugins ADD COLUMN origin TEXT;
ALTER TABLE vendor_plugins DROP CONSTRAINT vendor_plugins_source_check;
ALTER TABLE vendor_plugins ADD CONSTRAINT vendor_plugins_source_check CHECK (source IN ('builtin', 'local', 'marketplace'));

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
