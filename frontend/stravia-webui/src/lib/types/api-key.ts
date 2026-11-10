import type { ApiKeySpend } from './money'

export const TRANSPARENT_INJECTIONS = ['web_search', 'media_understanding', 'media_generation'] as const

export type TransparentInjection = (typeof TRANSPARENT_INJECTIONS)[number]

export interface ApiKey {
  id: string
  key: string
  name: string
  rpm_limit: number | null
  /** Decimal string in the current display currency; null means unlimited. */
  spend_limit?: string | null
  spend?: ApiKeySpend
  /** 剩余可消费金额（展示币种，已消费超出上限时为 "0"）；不限时为 null。 */
  spend_available?: string | null
  is_enabled: boolean
  mcp_access_enabled: boolean
  transparent_injection_enabled: boolean
  transparent_injections: TransparentInjection[]
  expires_at?: string | null
  created_at: string
  updated_at: string
  model_ids: string[]
}

export interface CreateApiKey {
  key?: string
  name: string
  rpm_limit?: number | null
  spend_limit?: string | null
  mcp_access_enabled: boolean
  transparent_injection_enabled: boolean
  transparent_injections: TransparentInjection[]
  expires_at?: string
  model_ids: string[]
}

export interface UpdateApiKey {
  key?: string
  name?: string
  rpm_limit?: number | null
  spend_limit?: string | null
  is_enabled?: boolean
  mcp_access_enabled?: boolean
  transparent_injection_enabled?: boolean
  transparent_injections?: TransparentInjection[]
  expires_at?: string
  model_ids?: string[]
}
