-- 凭据映射的有效期从最近一次规则真实检出起算；替换和还原不再续期。
ALTER TABLE reversible_redaction_mappings RENAME COLUMN last_used_at TO last_detected_at;

-- 旧的使用时间受到替换续期污染；存量只能以创建时间保守初始化，不能凭升级赠送有效期。
UPDATE reversible_redaction_mappings SET last_detected_at = created_at;

-- 扫描索引与水位改用检出时间，并重置水位以重新检查保留的存量。
DROP INDEX reversible_redaction_mappings_last_used_at_idx;
CREATE INDEX reversible_redaction_mappings_last_detected_at_idx
    ON reversible_redaction_mappings(last_detected_at);
UPDATE history_retention_watermarks
SET name = 'reversible_redaction_mappings.last_detected_at', position = -9223372036854775807
WHERE name = 'reversible_redaction_mappings.last_used_at';
