-- 凭据映射改为空闲（滑动）过期：保留期从「最近一次使用」起算，而不是从首次发布起算。
-- 列直接改名；published_at 保留，只表示「已发布过」。
ALTER TABLE reversible_redaction_mappings RENAME COLUMN updated_at TO last_used_at;

-- 升级既不缩短也不延长存量映射的剩余寿命：旧规则的期限是 published_at + N，
-- 所以已发布行的最近使用时刻取 published_at；更晚的 updated_at 不是一次「使用」记录，
-- 取它会让升级前已到期的映射复活。未发布行保持创建时刻，仍由 pending_until 回收。
UPDATE reversible_redaction_mappings
SET last_used_at = published_at
WHERE published_at IS NOT NULL;

-- 已发布行的回收改按 last_used_at 扫描，期限索引与持久化水位随之更换；
-- 水位重置为最小值，升级后第一次扫描把存量检查一遍。
DROP INDEX reversible_redaction_mappings_published_at_idx;
CREATE INDEX reversible_redaction_mappings_last_used_at_idx
    ON reversible_redaction_mappings(last_used_at);
UPDATE history_retention_watermarks
SET name = 'reversible_redaction_mappings.last_used_at', position = -9223372036854775807
WHERE name = 'reversible_redaction_mappings.published_at';
