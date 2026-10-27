-- 映射记下检测出它的规则：出站时已知凭据未必再被检测器命中，模型输出里的引用更没有检测上下文，
-- 命中记录只能靠映射自身携带的规则归因。既有映射没有归因信息，保持空数组。
ALTER TABLE reversible_redaction_mappings
    ADD COLUMN rule_ids text DEFAULT '[]'::text NOT NULL;
