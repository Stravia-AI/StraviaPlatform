-- 命中记录按创建事件逐行列出并在 sequence 上倒序翻页；复用事件不再被任何查询读取，
-- 部分索引只覆盖创建事件。
DROP INDEX IF EXISTS observation_events_credential_discovery_idx;
CREATE INDEX observation_events_credential_hit_idx
    ON observation_events(kind, sequence DESC)
    WHERE kind = 'credential_mappings_created';

-- 复用改为写入时登记：侧栏只按占位符做索引查找，不再解码事件载荷。
-- 主键即去重键（工具来源按 tool_id，用户消息按是否本次输入，其余来源共享空位置），
-- 只有更晚的复用能更新时间与 Run；interaction_id 索引供保留期清理与级联删除使用。
CREATE TABLE credential_mapping_reuses (
marker TEXT NOT NULL,
interaction_id TEXT NOT NULL REFERENCES interaction_observations(id) ON DELETE CASCADE,
run_id TEXT,
source_type TEXT NOT NULL,
tool_id TEXT NOT NULL DEFAULT '',
latest_input BOOLEAN NOT NULL DEFAULT false,
last_reused_at BIGINT NOT NULL,
PRIMARY KEY (marker, interaction_id, source_type, tool_id, latest_input)
);
CREATE INDEX credential_mapping_reuses_interaction_idx
    ON credential_mapping_reuses(interaction_id);

-- 已下线的 Kingfisher 规则不再出现在映射归因里；保持其余规则的原顺序。
UPDATE reversible_redaction_mappings AS m
SET rule_ids = COALESCE((
    SELECT jsonb_agg(value ORDER BY ordinality)::text
    FROM jsonb_array_elements_text(m.rule_ids::jsonb) WITH ORDINALITY AS rules(value, ordinality)
    WHERE left(value, 11) <> 'kingfisher.'
), '[]')
WHERE EXISTS (
    SELECT 1
    FROM jsonb_array_elements_text(m.rule_ids::jsonb) AS rules(value)
    WHERE left(value, 11) = 'kingfisher.'
);
