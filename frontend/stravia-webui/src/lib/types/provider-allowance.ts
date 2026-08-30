export type ProviderAllowanceStatus = 'fresh' | 'stale' | 'error'
export type AllowanceKind = 'quota_window' | 'request_allowance' | 'balance'
export type ProviderAllowanceErrorCategory =
  'authentication' | 'rate_limited' | 'timeout' | 'upstream_unavailable' | 'invalid_response'

export interface AllowanceAmount {
  value: number
  unit: string
  currency?: string
}

export interface Allowance {
  key: string
  label: string
  kind: AllowanceKind
  used?: AllowanceAmount
  remaining?: AllowanceAmount
  limit?: AllowanceAmount
  used_percent?: number
  window_seconds?: number
  reset_at?: number
}

export interface ModelAllowance {
  model: string
  allowances: Allowance[]
}

export interface ProviderAllowanceError {
  category: ProviderAllowanceErrorCategory
  message: string
}

export interface ProviderAllowanceSnapshot {
  provider_id: string
  provider_name: string
  catalog_provider_id: string
  channel: string
  plan_label?: string
  status: ProviderAllowanceStatus
  allowances: Allowance[]
  models: ModelAllowance[]
  fetched_at?: string
  error?: ProviderAllowanceError
}
