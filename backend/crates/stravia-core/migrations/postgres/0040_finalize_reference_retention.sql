-- 回填完成后移除旧到期列（PostgreSQL 删除列时随之删除引用它的索引）：依赖只在未发布/未确认时保留 pending_until，
-- 已发布/已确认的行由 published_at/confirmed_at 与当前天数计算下限窗口。
ALTER TABLE history_markers DROP COLUMN expires_at;
ALTER TABLE history_markers ADD CONSTRAINT history_markers_pending_until_check
    CHECK (published_at IS NULL OR pending_until IS NULL);

ALTER TABLE reversible_redaction_mappings DROP COLUMN expires_at;
ALTER TABLE reversible_redaction_mappings ADD CONSTRAINT reversible_redaction_mappings_pending_until_check
    CHECK (published_at IS NULL OR pending_until IS NULL);

ALTER TABLE native_compactions DROP COLUMN delivered_at;
ALTER TABLE native_compactions DROP COLUMN referenced_at;
ALTER TABLE native_compactions DROP COLUMN expires_at;
ALTER TABLE native_compactions ADD CONSTRAINT native_compactions_confirmation_check
    CHECK (confirmed_at IS NULL OR pending_until IS NULL);

-- 节点不再保存到期时间：读取只排除 deleting，回收只看 created_at + N。
ALTER TABLE turn_node_retention DROP COLUMN expires_at;

CREATE INDEX history_markers_pending_until_idx ON history_markers(pending_until);
CREATE INDEX history_markers_published_at_idx ON history_markers(published_at);

CREATE INDEX idx_reversible_redaction_mappings_principal_pending
ON reversible_redaction_mappings(principal, pending_until);
CREATE INDEX reversible_redaction_mappings_pending_until_idx
    ON reversible_redaction_mappings(pending_until);
CREATE INDEX reversible_redaction_mappings_published_at_idx
    ON reversible_redaction_mappings(published_at);

CREATE INDEX native_compactions_pending_until_idx ON native_compactions(pending_until);
CREATE INDEX native_compactions_confirmed_at_idx ON native_compactions(confirmed_at);

CREATE INDEX turn_nodes_created_at_idx ON turn_nodes(created_at, node_pk);

-- 每张引用关系在最后一条引用删除时把对象放进回收队列；外键级联删除同样触发。
--
-- 释放触发器先对被引用对象的行加 `FOR NO KEY UPDATE`，再检查剩余引用并入队：
-- 1. 回收方领取对象时持有 `FOR UPDATE`，触发器因此等待它提交，而不是在回收方即将
--    「仍有引用→移出队列」的同一刻因队列行冲突而漏登记；
-- 2. 两个事务同时删除同一对象的兄弟引用时互相等待，后者在新语句里看到前者已提交的
--    删除，因此总有一方登记，不会因彼此看不到对方尚未提交的删除而双双漏登记。
-- 写入方对对象只加 `FOR KEY SHARE`，与此锁不冲突。
CREATE FUNCTION turn_nodes_release_parent() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.parent_pk IS NOT NULL THEN
        PERFORM 1 FROM turn_nodes WHERE node_pk = OLD.parent_pk FOR NO KEY UPDATE;
        IF FOUND AND NOT EXISTS (SELECT 1 FROM turn_nodes WHERE parent_pk = OLD.parent_pk) THEN
            INSERT INTO turn_node_gc(node_pk) VALUES (OLD.parent_pk)
            ON CONFLICT (node_pk) DO NOTHING;
        END IF;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER turn_nodes_release_parent AFTER DELETE ON turn_nodes
    FOR EACH ROW EXECUTE FUNCTION turn_nodes_release_parent();

CREATE FUNCTION turn_node_markers_release() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM history_markers WHERE reference = OLD.reference FOR NO KEY UPDATE;
    IF FOUND AND NOT EXISTS (SELECT 1 FROM turn_node_markers WHERE reference = OLD.reference) THEN
        INSERT INTO history_marker_gc(reference) VALUES (OLD.reference)
        ON CONFLICT (reference) DO NOTHING;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER turn_node_markers_release AFTER DELETE ON turn_node_markers
    FOR EACH ROW EXECUTE FUNCTION turn_node_markers_release();

CREATE FUNCTION turn_node_redaction_mappings_release() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM reversible_redaction_mappings WHERE reference = OLD.reference FOR NO KEY UPDATE;
    IF FOUND AND NOT EXISTS (SELECT 1 FROM turn_node_redaction_mappings WHERE reference = OLD.reference) THEN
        INSERT INTO redaction_mapping_gc(reference) VALUES (OLD.reference)
        ON CONFLICT (reference) DO NOTHING;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER turn_node_redaction_mappings_release AFTER DELETE ON turn_node_redaction_mappings
    FOR EACH ROW EXECUTE FUNCTION turn_node_redaction_mappings_release();

CREATE FUNCTION turn_node_compactions_release() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM native_compactions WHERE id = OLD.record_id FOR NO KEY UPDATE;
    IF FOUND AND NOT EXISTS (SELECT 1 FROM turn_node_compactions WHERE record_id = OLD.record_id) THEN
        INSERT INTO native_compaction_gc(record_id) VALUES (OLD.record_id)
        ON CONFLICT (record_id) DO NOTHING;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER turn_node_compactions_release AFTER DELETE ON turn_node_compactions
    FOR EACH ROW EXECUTE FUNCTION turn_node_compactions_release();

CREATE FUNCTION native_compaction_sources_release() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.source_id <> OLD.record_id THEN
        PERFORM 1 FROM native_compactions WHERE id = OLD.source_id FOR NO KEY UPDATE;
        IF FOUND AND NOT EXISTS (SELECT 1 FROM native_compaction_sources WHERE source_id = OLD.source_id) THEN
            INSERT INTO native_compaction_gc(record_id) VALUES (OLD.source_id)
            ON CONFLICT (record_id) DO NOTHING;
        END IF;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER native_compaction_sources_release AFTER DELETE ON native_compaction_sources
    FOR EACH ROW EXECUTE FUNCTION native_compaction_sources_release();

CREATE FUNCTION native_compactions_release_source_node() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    IF OLD.source_generation_id IS NOT NULL THEN
        PERFORM 1 FROM turn_nodes WHERE id = OLD.source_generation_id FOR NO KEY UPDATE;
        IF FOUND AND NOT EXISTS (
            SELECT 1 FROM native_compactions c
            WHERE c.source_generation_id = OLD.source_generation_id
        ) THEN
            INSERT INTO turn_node_gc(node_pk)
            SELECT n.node_pk FROM turn_nodes n WHERE n.id = OLD.source_generation_id
            ON CONFLICT (node_pk) DO NOTHING;
        END IF;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER native_compactions_release_source_node AFTER DELETE ON native_compactions
    FOR EACH ROW EXECUTE FUNCTION native_compactions_release_source_node();

CREATE FUNCTION public_tool_executions_release_marker() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    PERFORM 1 FROM history_markers WHERE reference = OLD.marker_reference FOR NO KEY UPDATE;
    IF FOUND THEN
        INSERT INTO history_marker_gc(reference) VALUES (OLD.marker_reference)
        ON CONFLICT (reference) DO NOTHING;
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER public_tool_executions_release_marker AFTER DELETE ON public_tool_executions
    FOR EACH ROW EXECUTE FUNCTION public_tool_executions_release_marker();
