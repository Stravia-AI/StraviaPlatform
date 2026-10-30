-- 观测树只读取三类上下文事件；按 Interaction 和 sequence 定位快照内事件，
-- 避免为了少量上下文关联扫描同一 Interaction 的全部事件。
CREATE INDEX observation_events_context_kinds_idx
    ON observation_events(interaction_id, sequence)
    WHERE kind IN ('compaction_operation','native_compaction_associated','retained_tail_associated');
