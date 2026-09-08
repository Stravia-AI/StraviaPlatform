export * from './types/provider'
export * from './types/route'
export * from './types/api-key'
export * from './types/web'
export * from './types/stats'
export * from './types/observation'
export * from './types/oauth'
export * from './types/provider-allowance'

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

export interface CredentialDiscoveryQuery {
  cursor?: string
  limit?: number
}

export interface CredentialDiscoverySummary {
  interaction_id: string
  api_key_name: string | null
  discovered_at: number
  new_credential_count: number
  rule_ids: string[]
  source_types: string[]
  status: string
  observation_gap: boolean
}

export interface CredentialDiscoveryPage {
  items: CredentialDiscoverySummary[]
  next_cursor: string | null
  observation_gap: boolean
}
