import { describe, expect, test } from 'bun:test'
import {
  disabledRuleIdsAfterToggle,
  filterRuleRows,
  ruleSearchText,
  type RuleRow,
} from '../src/lib/credential-rule-catalog'
import type { CredentialRule, CustomCredentialRule } from '../src/lib/types'

function builtin(id: string, enabled = true): RuleRow {
  const rule: CredentialRule = {
    id,
    enabled,
    name: `Provider ${id}`,
    target: 'API tokens',
    description: 'Local credential detection',
    regex: null,
    path: null,
    secret_group: 0,
    keywords: ['TOKEN_PREFIX'],
    filter: '',
    components: [],
    skip_report: false,
    specificity: 0,
    confidence: 'high',
  }
  return { origin: 'builtin', id, name: rule.name, rule }
}

function custom(enabled = false): RuleRow {
  const rule: CustomCredentialRule = {
    id: 'custom.one',
    name: 'Internal service',
    description: 'Staging',
    enabled,
    spec: { mode: 'simple', text: 'do-not-index-this-secret' },
    created_at: 0,
    updated_at: 0,
  }
  return { origin: 'custom', id: rule.id, name: rule.name, rule }
}

describe('disabledRuleIdsAfterToggle', () => {
  const catalog = [
    { id: 'z', enabled: false },
    { id: 'a', enabled: true },
    { id: 'b', enabled: false },
  ]

  test('disabling preserves every other disabled rule, with sorted unique IDs', () => {
    expect(disabledRuleIdsAfterToggle(catalog, 'a', false)).toEqual(['a', 'b', 'z'])
    expect(disabledRuleIdsAfterToggle(catalog, 'z', false)).toEqual(['b', 'z'])
  })

  test('enabling removes only the selected rule and can produce an empty set', () => {
    expect(disabledRuleIdsAfterToggle(catalog, 'b', true)).toEqual(['z'])
    expect(disabledRuleIdsAfterToggle([{ id: 'b', enabled: false }], 'b', true)).toEqual([])
  })

  test('does not mutate the saved catalog', () => {
    disabledRuleIdsAfterToggle(catalog, 'z', true)
    expect(catalog[0]).toEqual({ id: 'z', enabled: false })
  })
})

describe('filterRuleRows', () => {
  const rows = [builtin('one'), builtin('two', false), custom()]
  const ids = (filtered: RuleRow[]) => filtered.map((row) => row.id)

  test('combines source and search filters and preserves input order', () => {
    expect(ids(filterRuleRows(rows, 'all', ''))).toEqual(['one', 'two', 'custom.one'])
    expect(ids(filterRuleRows(rows, 'all', 'API TOKENS'))).toEqual(['one', 'two'])
    expect(ids(filterRuleRows(rows, 'builtin', ' TOKEN_prefix '))).toEqual(['one', 'two'])
    expect(ids(filterRuleRows(rows, 'custom', 'STAGING'))).toEqual(['custom.one'])
    expect(ids(filterRuleRows(rows, 'builtin', 'staging'))).toEqual([])
  })

  test('custom secret text never participates in search', () => {
    expect(ruleSearchText(custom())).not.toContain('do-not-index-this-secret')
    expect(filterRuleRows(rows, 'all', 'do-not-index-this-secret')).toEqual([])
  })

  test('custom pattern expressions and keywords are also excluded', () => {
    const row = custom()
    if (row.origin !== 'custom') throw new Error('Expected a custom rule')
    row.rule.spec = {
      mode: 'pattern',
      regex: 'private-expression',
      keywords: ['private-keyword'],
      secret_group: 0,
      min_entropy: null,
    }
    expect(filterRuleRows([row], 'all', 'private')).toEqual([])
  })
})
