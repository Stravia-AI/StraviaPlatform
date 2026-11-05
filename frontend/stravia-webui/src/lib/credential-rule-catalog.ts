import type { CredentialRule, CustomCredentialRule } from '#lib/types.js'

// 内置与自定义规则共用浏览目录，编辑与保存仍由各自的接口负责。
export type RuleRow =
  | { origin: 'builtin'; id: string; name: string; rule: CredentialRule }
  | { origin: 'custom'; id: string; name: string; rule: CustomCredentialRule }

// 自定义规则的匹配文本可能就是凭据本身，不参与搜索索引。
export function ruleSearchText(row: RuleRow): string {
  return row.origin === 'builtin'
    ? `${row.name} ${row.id} ${row.rule.target} ${row.rule.description} ${row.rule.keywords.join(' ')}`
    : `${row.name} ${row.id} ${row.rule.description}`
}

export function filterRuleRows(rows: RuleRow[], origin: string, search: string): RuleRow[] {
  const needle = search.trim().toLocaleLowerCase()
  return rows.filter(
    (row) =>
      (origin === 'all' || row.origin === origin) &&
      (!needle || ruleSearchText(row).toLocaleLowerCase().includes(needle)),
  )
}

/** 接口替换整个集合；必须从完整目录而非当前筛选结果计算，保留其他规则的已保存状态。 */
export function disabledRuleIdsAfterToggle(
  rules: readonly Pick<CredentialRule, 'id' | 'enabled'>[],
  ruleId: string,
  enabled: boolean,
): string[] {
  const disabled = new Set(rules.filter((rule) => !rule.enabled).map((rule) => rule.id))
  if (enabled) disabled.delete(ruleId)
  else disabled.add(ruleId)
  return [...disabled].sort()
}
