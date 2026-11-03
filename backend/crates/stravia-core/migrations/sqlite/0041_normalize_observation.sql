-- ADR-0089：Observation 事件只保留顺序与关联列，kind 独有属性进 attrs；
-- thinking/visible 的块元数据独立成行；正文引用表由内容存储迁移（0037）创建。
-- 旧 payload / 到期列在数据转换完成后由 0042 删除。
ALTER TABLE observation_events ADD COLUMN attrs BLOB;

CREATE TABLE observation_blocks (
    sequence INTEGER PRIMARY KEY REFERENCES observation_events(sequence) ON DELETE CASCADE,
    interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES inference_run_observations(id) ON DELETE CASCADE,
    model_turn_id TEXT,
    attempt_id TEXT,
    item_ordinal BIGINT NOT NULL CHECK(item_ordinal >= 0),
    channel TEXT NOT NULL CHECK(channel IN ('thinking', 'visible')),
    complete INTEGER NOT NULL CHECK(complete IN (0, 1)),
    envelope BLOB
);
CREATE INDEX observation_blocks_channel_idx
    ON observation_blocks(interaction_id, channel, sequence DESC);

ALTER TABLE interaction_observations ADD COLUMN input_sequence BIGINT;

ALTER TABLE inference_run_observations ADD COLUMN inferred_retry INTEGER NOT NULL DEFAULT 0;
ALTER TABLE inference_run_observations ADD COLUMN grouping_reason TEXT;
ALTER TABLE inference_run_observations ADD COLUMN diagnostic_source_run_id TEXT;
ALTER TABLE inference_run_observations ADD COLUMN has_new_user INTEGER NOT NULL DEFAULT 0;
ALTER TABLE inference_run_observations ADD COLUMN superseded_by TEXT;

-- 扫描按活动时间取批次；到期列删除前先建立替代索引。
CREATE INDEX interaction_observations_activity_idx
    ON interaction_observations(last_active_at, id);

-- 一次性离线转换的断点：事件按 sequence 分批推进，中断后可继续。
CREATE TABLE observation_normalization_upgrade (
    phase INTEGER PRIMARY KEY CHECK(phase = 1),
    stage INTEGER NOT NULL CHECK(stage BETWEEN 0 AND 4),
    last_sequence BIGINT NOT NULL DEFAULT 0
);
INSERT INTO observation_normalization_upgrade(phase, stage, last_sequence) VALUES (1, 0, 0);
