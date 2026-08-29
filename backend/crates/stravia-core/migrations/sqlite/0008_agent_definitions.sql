CREATE TABLE agent_definition_revisions (
    definition_id TEXT NOT NULL,
    slug          TEXT NOT NULL,
    version       INTEGER NOT NULL CHECK (version > 0),
    spec_hash     TEXT NOT NULL,
    spec_json     TEXT NOT NULL CHECK (json_valid(spec_json)),
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (definition_id, version),
    UNIQUE (slug, version)
);

CREATE TABLE agent_definition_configs (
    definition_id TEXT PRIMARY KEY,
    enabled       INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    model_id      TEXT REFERENCES models(id) ON DELETE SET NULL,
    updated_at    INTEGER NOT NULL
);

CREATE INDEX idx_agent_definition_revisions_slug
    ON agent_definition_revisions(slug, version);
