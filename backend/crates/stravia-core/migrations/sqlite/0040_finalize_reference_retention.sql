-- 回填完成后移除旧到期列：依赖只在未发布/未确认时保留 pending_until，
-- 已发布/已确认的行由 published_at/confirmed_at 与当前天数计算下限窗口。
CREATE TABLE history_markers_reference (
    reference TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('platform', 'thinking')),
    activity TEXT NOT NULL,
    tool_id TEXT,
    call_payload TEXT,
    segment_payload TEXT,
    execution_state TEXT CHECK (
        execution_state IS NULL OR
        execution_state IN ('pending', 'running', 'completed', 'failed', 'interrupted')
    ),
    execution_owner TEXT,
    lease_expires_at INTEGER,
    execution_deadline INTEGER,
    published_at INTEGER,
    pending_until INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (published_at IS NULL OR pending_until IS NULL),
    CHECK (
        (kind = 'thinking' AND tool_id IS NULL AND call_payload IS NULL
         AND execution_state IS NULL AND segment_payload IS NOT NULL)
        OR
        (kind = 'platform' AND tool_id IS NOT NULL AND call_payload IS NOT NULL
         AND execution_state IS NOT NULL AND execution_deadline IS NOT NULL)
    )
);
INSERT INTO history_markers_reference
    (reference, principal, kind, activity, tool_id, call_payload, segment_payload,
     execution_state, execution_owner, lease_expires_at, execution_deadline,
     published_at, pending_until, created_at, updated_at)
    SELECT reference, principal, kind, activity, tool_id, call_payload, segment_payload,
           execution_state, execution_owner, lease_expires_at, execution_deadline,
           published_at, pending_until, created_at, updated_at
    FROM history_markers;
DROP TABLE history_markers;
ALTER TABLE history_markers_reference RENAME TO history_markers;

CREATE INDEX idx_history_markers_execution
ON history_markers(execution_state, lease_expires_at, execution_deadline);
CREATE INDEX idx_history_markers_principal_reference
ON history_markers(principal, reference);
CREATE INDEX history_markers_pending_until_idx ON history_markers(pending_until);
CREATE INDEX history_markers_published_at_idx ON history_markers(published_at);

CREATE TABLE reversible_redaction_mappings_reference (
    reference TEXT PRIMARY KEY NOT NULL,
    principal TEXT NOT NULL,
    secret TEXT NOT NULL,
    published_at INTEGER,
    pending_until INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    rule_ids TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(rule_ids) AND json_type(rule_ids) = 'array'),
    CHECK (published_at IS NULL OR pending_until IS NULL)
);
INSERT INTO reversible_redaction_mappings_reference
    (reference, principal, secret, published_at, pending_until, created_at, updated_at, rule_ids)
    SELECT reference, principal, secret, published_at, pending_until, created_at, updated_at, rule_ids
    FROM reversible_redaction_mappings;
DROP TABLE reversible_redaction_mappings;
ALTER TABLE reversible_redaction_mappings_reference RENAME TO reversible_redaction_mappings;

CREATE INDEX idx_reversible_redaction_mappings_principal_expiry
ON reversible_redaction_mappings(principal, pending_until);
CREATE INDEX reversible_redaction_mappings_pending_until_idx
    ON reversible_redaction_mappings(pending_until);
CREATE INDEX reversible_redaction_mappings_published_at_idx
    ON reversible_redaction_mappings(published_at);

CREATE TABLE native_compactions_reference (
    id TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    source_generation_id TEXT REFERENCES turn_nodes(id) ON DELETE RESTRICT,
    operation_id TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at INTEGER NOT NULL,
    confirmed_at INTEGER,
    pending_until INTEGER,
    CHECK (confirmed_at IS NULL OR pending_until IS NULL)
);
INSERT INTO native_compactions_reference
    (id, principal, source_generation_id, operation_id, payload, created_at,
     confirmed_at, pending_until)
    SELECT id, principal, source_generation_id, operation_id, payload, created_at,
           confirmed_at, pending_until
    FROM native_compactions;
DROP TABLE native_compactions;
ALTER TABLE native_compactions_reference RENAME TO native_compactions;

CREATE INDEX native_compactions_generation_idx ON native_compactions(source_generation_id)
    WHERE source_generation_id IS NOT NULL;
CREATE INDEX native_compactions_pending_until_idx ON native_compactions(pending_until);
CREATE INDEX native_compactions_confirmed_at_idx ON native_compactions(confirmed_at);

-- 节点不再保存到期时间：读取只排除 deleting，回收只看 created_at + N。
CREATE TABLE turn_node_retention_reference (
    node_pk INTEGER PRIMARY KEY REFERENCES turn_nodes(node_pk) ON DELETE CASCADE,
    deleting INTEGER NOT NULL DEFAULT 0 CHECK(deleting IN (0, 1))
);
INSERT INTO turn_node_retention_reference(node_pk, deleting)
    SELECT node_pk, deleting FROM turn_node_retention;
DROP TABLE turn_node_retention;
ALTER TABLE turn_node_retention_reference RENAME TO turn_node_retention;

CREATE INDEX turn_nodes_created_at_idx ON turn_nodes(created_at, node_pk);

-- 每张引用关系在最后一条引用删除时把对象放进回收队列；外键级联删除同样触发。
CREATE TRIGGER turn_nodes_release_parent AFTER DELETE ON turn_nodes
BEGIN
    INSERT OR IGNORE INTO turn_node_gc(node_pk)
    SELECT OLD.parent_pk
    WHERE OLD.parent_pk IS NOT NULL
      AND EXISTS (SELECT 1 FROM turn_nodes WHERE node_pk = OLD.parent_pk)
      AND NOT EXISTS (SELECT 1 FROM turn_nodes WHERE parent_pk = OLD.parent_pk);
END;

CREATE TRIGGER turn_node_markers_release AFTER DELETE ON turn_node_markers
BEGIN
    INSERT OR IGNORE INTO history_marker_gc(reference)
    SELECT OLD.reference
    WHERE EXISTS (SELECT 1 FROM history_markers WHERE reference = OLD.reference)
      AND NOT EXISTS (SELECT 1 FROM turn_node_markers WHERE reference = OLD.reference);
END;

CREATE TRIGGER turn_node_redaction_mappings_release AFTER DELETE ON turn_node_redaction_mappings
BEGIN
    INSERT OR IGNORE INTO redaction_mapping_gc(reference)
    SELECT OLD.reference
    WHERE EXISTS (SELECT 1 FROM reversible_redaction_mappings WHERE reference = OLD.reference)
      AND NOT EXISTS (SELECT 1 FROM turn_node_redaction_mappings WHERE reference = OLD.reference);
END;

CREATE TRIGGER turn_node_compactions_release AFTER DELETE ON turn_node_compactions
BEGIN
    INSERT OR IGNORE INTO native_compaction_gc(record_id)
    SELECT OLD.record_id
    WHERE EXISTS (SELECT 1 FROM native_compactions WHERE id = OLD.record_id)
      AND NOT EXISTS (SELECT 1 FROM turn_node_compactions WHERE record_id = OLD.record_id);
END;

CREATE TRIGGER native_compaction_sources_release AFTER DELETE ON native_compaction_sources
BEGIN
    INSERT OR IGNORE INTO native_compaction_gc(record_id)
    SELECT OLD.source_id
    WHERE OLD.source_id <> OLD.record_id
      AND EXISTS (SELECT 1 FROM native_compactions WHERE id = OLD.source_id)
      AND NOT EXISTS (SELECT 1 FROM native_compaction_sources WHERE source_id = OLD.source_id);
END;

CREATE TRIGGER native_compactions_release_source_node AFTER DELETE ON native_compactions
BEGIN
    INSERT OR IGNORE INTO turn_node_gc(node_pk)
    SELECT n.node_pk
    FROM turn_nodes n
    WHERE n.id = OLD.source_generation_id
      AND NOT EXISTS (
          SELECT 1 FROM native_compactions c
          WHERE c.source_generation_id = OLD.source_generation_id
      );
END;

CREATE TRIGGER public_tool_executions_release_marker AFTER DELETE ON public_tool_executions
BEGIN
    INSERT OR IGNORE INTO history_marker_gc(reference)
    SELECT OLD.marker_reference
    WHERE EXISTS (SELECT 1 FROM history_markers WHERE reference = OLD.marker_reference);
END;
