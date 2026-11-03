-- ADR-0089：Observation 事件只保留顺序与关联列，kind 独有属性进 attrs；
-- thinking/visible 的块元数据独立成行；正文引用表由内容存储迁移（0037）创建。
-- 旧 payload / 到期列在数据转换完成后由 0042 删除。
ALTER TABLE observation_events ADD COLUMN attrs bytea;

CREATE TABLE observation_blocks (
    sequence bigint PRIMARY KEY REFERENCES observation_events(sequence) ON DELETE CASCADE,
    interaction_id text NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    run_id text NOT NULL REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    model_turn_id text,
    attempt_id text,
    item_ordinal bigint NOT NULL CHECK(item_ordinal >= 0),
    channel text NOT NULL CHECK(channel IN ('thinking', 'visible')),
    complete boolean NOT NULL,
    envelope bytea
);
CREATE INDEX observation_blocks_channel_idx
    ON observation_blocks(interaction_id, channel, sequence DESC);

ALTER TABLE interaction_observations ADD COLUMN input_sequence bigint;

ALTER TABLE inference_run_observations ADD COLUMN inferred_retry boolean NOT NULL DEFAULT false;
ALTER TABLE inference_run_observations ADD COLUMN grouping_reason text;
ALTER TABLE inference_run_observations ADD COLUMN diagnostic_source_run_id text;
ALTER TABLE inference_run_observations ADD COLUMN has_new_user boolean NOT NULL DEFAULT false;
ALTER TABLE inference_run_observations ADD COLUMN superseded_by text;

CREATE INDEX interaction_observations_activity_idx
    ON interaction_observations(last_active_at, id);

CREATE TABLE observation_normalization_upgrade (
    phase integer PRIMARY KEY CHECK (phase = 1),
    stage bigint NOT NULL CHECK (stage BETWEEN 0 AND 4),
    last_sequence bigint NOT NULL DEFAULT 0
);
INSERT INTO observation_normalization_upgrade(phase, stage, last_sequence) VALUES (1, 0, 0);
