CREATE TABLE agent_definition_revisions (
    definition_id TEXT NOT NULL,
    slug          TEXT NOT NULL,
    version       BIGINT NOT NULL CHECK (version > 0),
    spec_hash     TEXT NOT NULL,
    spec_json     TEXT NOT NULL,
    created_at    BIGINT NOT NULL,
    PRIMARY KEY (definition_id, version),
    UNIQUE (slug, version)
);

CREATE TABLE agent_definition_configs (
    definition_id TEXT PRIMARY KEY,
    enabled       BOOLEAN NOT NULL DEFAULT FALSE,
    model_id      TEXT REFERENCES models(id) ON DELETE SET NULL,
    updated_at    BIGINT NOT NULL
);

CREATE INDEX idx_agent_definition_revisions_slug
    ON agent_definition_revisions(slug, version);
