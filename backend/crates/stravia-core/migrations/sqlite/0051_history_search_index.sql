CREATE TABLE history_search_segments (
    id INTEGER PRIMARY KEY,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    principal TEXT NOT NULL,
    root_id TEXT NOT NULL,
    event_sequence BIGINT NOT NULL,
    segment_ordinal BIGINT NOT NULL,
    role_weight INTEGER NOT NULL,
    occurred_at BIGINT NOT NULL,
    byte_start BIGINT NOT NULL,
    byte_end BIGINT NOT NULL,
    UNIQUE(event_sequence, segment_ordinal)
);
CREATE INDEX history_search_segments_interaction_idx ON history_search_segments(interaction_id);
CREATE INDEX history_search_segments_scope_idx ON history_search_segments(principal, root_id, interaction_id);

CREATE VIRTUAL TABLE history_search_segments_fts USING fts5(
    tokens, content='', contentless_delete=1, tokenize='ascii'
);
CREATE TRIGGER history_search_segments_delete AFTER DELETE ON history_search_segments BEGIN
    DELETE FROM history_search_segments_fts WHERE rowid = old.id;
END;

CREATE TABLE history_search_progress (
    interaction_id TEXT PRIMARY KEY REFERENCES interaction_observations(id) ON DELETE CASCADE,
    last_event_sequence BIGINT NOT NULL DEFAULT 0,
    indexed_terminal INTEGER NOT NULL DEFAULT 0 CHECK(indexed_terminal IN (0, 1))
);
