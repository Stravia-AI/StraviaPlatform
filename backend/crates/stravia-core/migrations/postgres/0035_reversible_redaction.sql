CREATE TABLE reversible_redaction_mappings (
    reference TEXT PRIMARY KEY NOT NULL,
    principal TEXT NOT NULL,
    secret TEXT NOT NULL,
    published_at BIGINT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL
);

CREATE INDEX idx_reversible_redaction_mappings_principal_expiry
ON reversible_redaction_mappings(principal, expires_at);

CREATE INDEX idx_reversible_redaction_mappings_expiry
ON reversible_redaction_mappings(expires_at);
