-- 共享正文存储（收尾）：切换 Turn Chain 引用表，删除旧正文表，Marker 段载荷改为编码外壳。
-- 升级钩子已在 0037 与 0038 之间完成数据转换；本文件不读取业务正文。
DROP TABLE turn_node_contents;
ALTER TABLE turn_node_contents_v4 RENAME TO turn_node_contents;
DROP INDEX turn_node_contents_v4_content_idx;
CREATE INDEX turn_node_contents_content_idx ON turn_node_contents(content_id);

DROP TABLE turn_content_gc;
DROP TABLE turn_contents;

-- 旧二进制不能读取新 schema；回退只能恢复升级前的备份。
-- SQLite 无法原地改列类型，且 segment_payload 出现在 CHECK 约束中，只能重建。
-- 此时外键已由迁移 runner 关闭，重建不会级联删除 history_marker_contents。
CREATE TABLE history_markers_v4 (
    reference TEXT PRIMARY KEY,
    principal TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('platform', 'thinking')),
    activity TEXT NOT NULL,
    tool_id TEXT,
    call_payload TEXT,
    segment_payload BLOB,
    execution_state TEXT CHECK (
        execution_state IS NULL OR
        execution_state IN ('pending', 'running', 'completed', 'failed', 'interrupted')
    ),
    execution_owner TEXT,
    lease_expires_at INTEGER,
    execution_deadline INTEGER,
    published_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    CHECK (
        (kind = 'thinking' AND tool_id IS NULL AND call_payload IS NULL
         AND execution_state IS NULL AND segment_payload IS NOT NULL)
        OR
        (kind = 'platform' AND tool_id IS NOT NULL AND call_payload IS NOT NULL
         AND execution_state IS NOT NULL AND execution_deadline IS NOT NULL)
    )
);
INSERT INTO history_markers_v4
    (reference, principal, kind, activity, tool_id, call_payload, segment_payload,
     execution_state, execution_owner, lease_expires_at, execution_deadline,
     published_at, created_at, updated_at, expires_at)
SELECT reference, principal, kind, activity, tool_id, call_payload, segment_payload_encoded,
       execution_state, execution_owner, lease_expires_at, execution_deadline,
       published_at, created_at, updated_at, expires_at
FROM history_markers;
DROP TABLE history_markers;
ALTER TABLE history_markers_v4 RENAME TO history_markers;
CREATE INDEX idx_history_markers_execution
ON history_markers(execution_state, lease_expires_at, execution_deadline);
CREATE INDEX idx_history_markers_expiry
ON history_markers(expires_at);
CREATE INDEX idx_history_markers_principal_reference
ON history_markers(principal, reference);
-- updated_at is captured by the terminal write, not by subsequent publication.
CREATE TRIGGER retain_terminal_tool_execution AFTER UPDATE OF execution_state ON history_markers
WHEN OLD.execution_state IN ('pending','running') AND NEW.execution_state IN ('completed','failed','interrupted')
BEGIN
    UPDATE public_tool_executions
    SET terminal_at = NEW.updated_at,
        retention_started_at = NEW.updated_at,
        retention_ms = COALESCE((SELECT CAST(value AS BIGINT) * 86400000 FROM settings WHERE name = 'log_retention_days'), retention_ms),
        expires_at = NEW.updated_at + COALESCE((SELECT CAST(value AS BIGINT) * 86400000 FROM settings WHERE name = 'log_retention_days'), retention_ms)
    WHERE marker_reference = NEW.reference AND retention_started_at IS NULL;
END;

DROP TABLE content_upgrade;
