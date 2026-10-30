export * from './types/provider'
export * from './types/route'
export * from './types/api-key'
export * from './types/web'
export * from './types/stats'
export * from './types/observation'
export * from './types/oauth'
export * from './types/provider-allowance'
export * from './types/media-generation'
export * from './types/plugin'
export * from './types/money'

export interface CredentialRule {
  id: string
  name: string
  target: string
  description: string
  regex: string | null
  path: string | null
  secret_group: number
  keywords: string[]
  filter: string
  components: { id: string; within: string; optional: boolean }[]
  skip_report: boolean
  specificity: number
  confidence: string
}

export type CustomCredentialRuleSpec =
  | { mode: 'simple'; text: string }
  | { mode: 'pattern'; regex: string; secret_group: number; keywords: string[]; min_entropy: number | null }

export interface CustomCredentialRule {
  id: string
  name: string
  description: string
  enabled: boolean
  spec: CustomCredentialRuleSpec
  created_at: number
  updated_at: number
}

export interface CustomCredentialRuleInput {
  name: string
  description: string
  enabled: boolean
  spec: CustomCredentialRuleSpec
}

export interface CredentialRuleCatalog {
  rules: CredentialRule[]
  prefilter: string
  filter: string
}

export interface CredentialMatch {
  rule_id: string
  start: number
  end: number
  start_line: number
  start_column: number
  end_line: number
  end_column: number
}

export type CredentialRuleSet = 'builtin' | 'custom'

export interface CredentialDiscoveryQuery {
  cursor?: string
  limit?: number
  /** 创建时间下限（含），Unix 毫秒。 */
  start_at?: number
  /** 创建时间上限（不含），Unix 毫秒。 */
  end_at?: number
}

/** 一次新建映射的命中位置；`latest_input` 仅用户消息有意义，`false` 表示客户端重放的历史。 */
export interface CredentialHit {
  rule_id: string
  source_type: string
  tool_id?: string
  latest_input?: boolean
}

export interface CredentialDiscoveryRecord {
  /** `"{event.sequence}.{discovery index}"`，同一映射的稳定标识。 */
  id: string
  created_at: number
  interaction_id: string
  run_id: string | null
  api_key_name: string | null
  rule_sets: CredentialRuleSet[]
  rule_ids: string[]
  hits: CredentialHit[]
  omitted_hits: number
  linkable: boolean
}

export interface CredentialDiscoveryPage {
  items: CredentialDiscoveryRecord[]
  next_cursor: string | null
}

export interface CredentialHitField {
  source_type: string
  tool_id?: string | null
  tool_name?: string | null
}

export interface CredentialHitMatch extends CredentialMatch {
  /** 该匹配是否就是本行映射对应的命中。 */
  hit: boolean
}

export type CredentialHitDetail =
  | {
      state: 'available'
      field: CredentialHitField
      text: string
      truncated: boolean
      hit_basis: 'secret' | 'rule'
      matches: CredentialHitMatch[]
    }
  | { state: 'unavailable'; field: CredentialHitField; reason: 'not_retained' | 'missing' }

export interface CredentialHitReuse {
  interaction_id: string
  run_id: string | null
  api_key_name: string | null
  source_type: string
  tool_id?: string | null
  latest_input?: boolean | null
  last_reused_at: number
}

export interface CredentialHitReuses {
  linkable: boolean
  items: CredentialHitReuse[]
  truncated: boolean
}
