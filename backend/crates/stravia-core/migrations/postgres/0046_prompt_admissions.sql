CREATE TABLE prompt_admission_epoch (id INTEGER PRIMARY KEY CHECK (id = 1), epoch BIGINT NOT NULL);
INSERT INTO prompt_admission_epoch(id, epoch) VALUES (1, 0);
CREATE TABLE prompt_admissions (
    id TEXT PRIMARY KEY NOT NULL,
    principal TEXT NOT NULL,
    parent_id TEXT,
    atomic_lineage_created BOOLEAN NOT NULL DEFAULT FALSE,
    ordinal BIGINT NOT NULL CHECK (ordinal >= 0),
    user_turn_index BIGINT CHECK (user_turn_index >= 0),
    dialogue_prompt_index BIGINT CHECK (dialogue_prompt_index >= 0),
    history_count BIGINT NOT NULL CHECK (history_count >= 0),
    prefix_digest TEXT NOT NULL,
    controls_digest TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    conversation_id TEXT NOT NULL,
    conversation_created_at_ms BIGINT NOT NULL,
    user_prompt_id TEXT,
    user_prompt_created_at_ms BIGINT,
    logical_turn_id TEXT NOT NULL,
    logical_turn_created_at_ms BIGINT NOT NULL,
    context_window_id TEXT,
    context_window_index BIGINT CHECK (context_window_index >= 0),
    CHECK ((ordinal = 0 AND user_prompt_id IS NULL AND user_prompt_created_at_ms IS NULL) OR (ordinal > 0 AND user_prompt_id IS NOT NULL AND user_prompt_created_at_ms IS NOT NULL)),
    CHECK (ordinal = 0 OR (logical_turn_id = user_prompt_id AND logical_turn_created_at_ms = user_prompt_created_at_ms)),
    CHECK ((context_window_id IS NULL) = (context_window_index IS NULL)),
    UNIQUE (principal, id),
    FOREIGN KEY (principal, parent_id) REFERENCES prompt_admissions(principal, id)
);
CREATE INDEX prompt_admissions_prefix_idx ON prompt_admissions(principal, controls_digest, history_count, prefix_digest);
CREATE INDEX prompt_admissions_parent_idx ON prompt_admissions(parent_id);
CREATE INDEX prompt_admissions_created_idx ON prompt_admissions(created_at_ms);
