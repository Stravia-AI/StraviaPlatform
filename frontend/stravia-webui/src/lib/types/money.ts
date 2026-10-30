/** Currency chosen for the instance display; Core freezes and converts amounts, the UI only renders them. */
export type CurrencyKind = 'usd' | 'custom'

export interface DisplayCurrency {
  kind: CurrencyKind
  symbol: string
}

/** Instance custom currency. All decimal values are strings. */
export interface CustomCurrency {
  symbol: string
  /** 1 USD = X custom currency units. */
  usd_rate: string
  /** Rate frozen when the custom currency was first configured. */
  first_rate: string
  configured_at: string
}

/** Money amount already converted by Core into the display currency. */
export interface MoneyAmount {
  amount: string
  known_count: number
  unknown_count: number
}

/** A single frozen amount attached to one attempt or configured price. */
export interface DisplayAmount {
  amount: string
  currency: DisplayCurrency
}

export interface ProviderCostPlanPrice {
  amount: string
  currency: CurrencyKind
}

export type CostBasis = 'none' | 'usage' | 'subscription'
export type SubscriptionPeriod = 'month' | 'year'

export interface ProviderCostPlan {
  id: string
  provider_id: string
  /** UTC epoch milliseconds. */
  effective_from: number
  basis: CostBasis
  period: SubscriptionPeriod | null
  price: ProviderCostPlanPrice | null
  created_at: string
}

export type NewProviderCostPlan =
  | { basis: 'none' }
  | { basis: 'usage' }
  | { basis: 'subscription'; period: SubscriptionPeriod; price: ProviderCostPlanPrice }

export interface ApiKeySpend {
  /** Spent amount in the current display currency, decimal string. */
  spent: string
  reset_at: string | null
}

export type AttemptBillingState = 'charged' | 'unknown'
