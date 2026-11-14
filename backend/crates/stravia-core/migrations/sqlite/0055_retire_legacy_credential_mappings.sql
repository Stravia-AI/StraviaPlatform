-- 无归因或已下线规则的映射不再保留历史还原能力；前置钩子已移除退役归因。
-- 先解除节点引用，再删除映射及复用登记；整份 SQL 在同一迁移事务中执行。
DELETE FROM turn_node_redaction_mappings
WHERE reference IN (
    SELECT reference FROM reversible_redaction_mappings WHERE json_array_length(rule_ids) = 0
);
DELETE FROM credential_mapping_reuses
WHERE marker IN (
    SELECT reference FROM reversible_redaction_mappings WHERE json_array_length(rule_ids) = 0
);
DELETE FROM reversible_redaction_mappings WHERE json_array_length(rule_ids) = 0;
