-- 依赖改为按引用保留：新增节点侧引用表、回收队列、期限水位与可空的下限列。
-- 本迁移只做增量；启动钩子回填引用与首次发布/确认时刻后，0040 才移除旧到期列。
ALTER TABLE history_markers ADD COLUMN pending_until bigint;
ALTER TABLE reversible_redaction_mappings ADD COLUMN pending_until bigint;
ALTER TABLE native_compactions ADD COLUMN pending_until bigint;
ALTER TABLE native_compactions ADD COLUMN confirmed_at bigint;

CREATE TABLE turn_node_markers (
    node_pk bigint NOT NULL REFERENCES turn_nodes(node_pk) ON DELETE CASCADE,
    reference text NOT NULL REFERENCES history_markers(reference) ON DELETE RESTRICT,
    PRIMARY KEY (node_pk, reference)
);
CREATE INDEX turn_node_markers_reference_idx ON turn_node_markers(reference);

CREATE TABLE turn_node_redaction_mappings (
    node_pk bigint NOT NULL REFERENCES turn_nodes(node_pk) ON DELETE CASCADE,
    reference text NOT NULL REFERENCES reversible_redaction_mappings(reference) ON DELETE RESTRICT,
    PRIMARY KEY (node_pk, reference)
);
CREATE INDEX turn_node_redaction_mappings_reference_idx
    ON turn_node_redaction_mappings(reference);

CREATE TABLE turn_node_compactions (
    node_pk bigint NOT NULL REFERENCES turn_nodes(node_pk) ON DELETE CASCADE,
    record_id text NOT NULL REFERENCES native_compactions(id) ON DELETE RESTRICT,
    PRIMARY KEY (node_pk, record_id)
);
CREATE INDEX turn_node_compactions_record_idx ON turn_node_compactions(record_id);

-- 队列行随对象删除级联；触发器和期限扫描把候选放进这里，回收是唯一的删除方。
CREATE TABLE history_marker_gc (
    reference text PRIMARY KEY REFERENCES history_markers(reference) ON DELETE CASCADE
);
CREATE TABLE redaction_mapping_gc (
    reference text PRIMARY KEY REFERENCES reversible_redaction_mappings(reference) ON DELETE CASCADE
);
CREATE TABLE native_compaction_gc (
    record_id text PRIMARY KEY REFERENCES native_compactions(id) ON DELETE CASCADE
);

-- 每个期限列一个持久化水位；初值为最小值，升级后第一次扫描检查全部存量一次。
CREATE TABLE history_retention_watermarks (
    name text PRIMARY KEY,
    position bigint NOT NULL
);
INSERT INTO history_retention_watermarks(name, position) VALUES
    ('turn_nodes.created_at', -9223372036854775807),
    ('history_markers.pending_until', -9223372036854775807),
    ('history_markers.published_at', -9223372036854775807),
    ('reversible_redaction_mappings.pending_until', -9223372036854775807),
    ('reversible_redaction_mappings.published_at', -9223372036854775807),
    ('native_compactions.pending_until', -9223372036854775807),
    ('native_compactions.confirmed_at', -9223372036854775807);
