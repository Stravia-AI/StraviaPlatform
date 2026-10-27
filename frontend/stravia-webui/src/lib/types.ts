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

export type CredentialHitKind = 'all' | 'created' | 'reused'

export interface CredentialDiscoveryQuery {
  cursor?: string
  limit?: number
  kind?: CredentialHitKind
}

/** 一个「规则 ← 来源」配对；`latest_input` 仅用户消息有意义，`false` 表示客户端重放的历史。 */
export interface CredentialHit {
  rule_id: string
  source_type: string
  latest_input?: boolean
}

export interface CredentialDiscoverySummary {
  interaction_id: string
  api_key_name: string | null
  /** 当前筛选下该 Interaction 最近一次命中的时间。 */
  discovered_at: number
  new_credential_count: number
  created: CredentialHit[]
  reused: CredentialHit[]
  status: string
  observation_gap: boolean
}

export interface CredentialDiscoveryPage {
  items: CredentialDiscoverySummary[]
  next_cursor: string | null
}
