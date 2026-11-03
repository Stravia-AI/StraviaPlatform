-- 共享正文存储（收尾）：切换 Turn Chain 引用表，删除旧正文表，Marker 段载荷改为编码外壳。
-- 升级钩子已在 0037 与 0038 之间完成数据转换；本文件不读取业务正文。
DROP TABLE turn_node_contents;
ALTER TABLE turn_node_contents_v4 RENAME TO turn_node_contents;
DROP INDEX turn_node_contents_v4_content_idx;
CREATE INDEX turn_node_contents_content_idx ON turn_node_contents(content_id);
ALTER INDEX turn_node_contents_v4_pkey RENAME TO turn_node_contents_pkey;
ALTER TABLE turn_node_contents RENAME CONSTRAINT turn_node_contents_v4_node_pk_content_id_key
    TO turn_node_contents_node_pk_content_id_key;
ALTER TABLE turn_node_contents RENAME CONSTRAINT turn_node_contents_v4_node_pk_scope_id_fkey
    TO turn_node_contents_node_pk_scope_id_fkey;
ALTER TABLE turn_node_contents RENAME CONSTRAINT turn_node_contents_v4_scope_id_content_id_fkey
    TO turn_node_contents_scope_id_content_id_fkey;

DROP TABLE turn_content_gc;
DROP TABLE turn_contents;

-- 旧二进制不能读取新 schema；回退只能恢复升级前的备份。
-- CHECK 约束引用 segment_payload，删除列会连带删除约束，因此随后以同一名称重建。
ALTER TABLE history_markers DROP CONSTRAINT history_markers_check;
ALTER TABLE history_markers DROP COLUMN segment_payload;
ALTER TABLE history_markers RENAME COLUMN segment_payload_encoded TO segment_payload;
ALTER TABLE history_markers ALTER COLUMN segment_payload SET STORAGE EXTERNAL;
ALTER TABLE history_markers ADD CONSTRAINT history_markers_check CHECK (
    ((kind = 'thinking'::text) AND (tool_id IS NULL) AND (call_payload IS NULL)
        AND (execution_state IS NULL) AND (segment_payload IS NOT NULL))
    OR ((kind = 'platform'::text) AND (tool_id IS NOT NULL) AND (call_payload IS NOT NULL)
        AND (execution_state IS NOT NULL) AND (execution_deadline IS NOT NULL))
);

DROP TABLE content_upgrade;
