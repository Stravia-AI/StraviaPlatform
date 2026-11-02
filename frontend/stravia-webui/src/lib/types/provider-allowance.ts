export type ProviderAllowanceStatus = 'fresh' | 'stale' | 'error'
export type AllowanceKind = 'quota_window' | 'request_allowance' | 'balance'
export type AllowanceCondition = 'normal' | 'tight' | 'exhausted'
export type ExhaustionForecastStatus = 'no_risk' | 'will_exhaust' | 'unknown'
export type ProviderAllowanceErrorCategory =
  'authentication' | 'rate_limited' | 'timeout' | 'upstream_unavailable' | 'invalid_response'

export type ProviderAllowanceBillingKind = 'usage' | 'subscription'

/** 供应商插件识别出的计费方式，只是成本方式的建议；价格是一个周期的官方标价，币种为 ISO 4217 代码。 */
export interface ProviderAllowanceBilling {
  kind: ProviderAllowanceBillingKind
  period?: 'month' | 'year'
  price?: { amount: string; currency: string }
}

export interface AllowanceAmount {
  value: number
  unit: string
  currency?: string
}

export interface Allowance {
  key: string
  guarded: boolean
  label: string
  kind: AllowanceKind
  used?: AllowanceAmount
  remaining?: AllowanceAmount
  limit?: AllowanceAmount
  used_percent?: number
  window_seconds?: number
  reset_at?: number
  condition?: AllowanceCondition
  forecast: { status: ExhaustionForecastStatus; projected_remaining_percent?: number; exhausts_at?: number }
}

export interface ModelAllowance {
  model: string
  allowances: Allowance[]
}

export interface ProviderAllowanceError {
  category: ProviderAllowanceErrorCategory
  message: string
}

export interface ProviderAllowanceTarget {
  provider_id: string
  guard_supported: boolean
  suspension: AllowanceSuspension | null
  provider_name: string
  catalog_provider_id: string
  channel: string
  snapshot?: ProviderAllowanceSnapshot
  refreshing: boolean
}

export interface ProviderAllowanceGuardUpdate {
  keys: string[]
  target: ProviderAllowanceTarget
}

export interface ProviderAllowanceSnapshot {
  provider_id: string
  guard_supported: boolean
  missing_guarded_keys: string[]
  suspension: AllowanceSuspension | null
  provider_name: string
  catalog_provider_id: string
  channel: string
  plan_label?: string
  account_label?: string
  billing?: ProviderAllowanceBilling
  status: ProviderAllowanceStatus
  allowances: Allowance[]
  models: ModelAllowance[]
  fetched_at?: string
  error?: ProviderAllowanceError
}

export interface AllowanceSuspension {
  suspended_at: string
  triggered_keys: string[]
  earliest_reset_at: number | null
}
