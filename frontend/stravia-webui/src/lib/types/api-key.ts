import type { ApiKeySpend } from './money'

export interface ApiKey {
  id: string
  key: string
  name: string
  rpm_limit: number | null
  /** Decimal string in the current display currency; null means unlimited. */
  spend_limit?: string | null
  spend?: ApiKeySpend
  is_enabled: boolean
  mcp_access_enabled: boolean
  transparent_injection_enabled: boolean
  inject_web_search: boolean
  inject_media_understanding: boolean
  inject_media_generation: boolean
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
  inject_web_search: boolean
  inject_media_understanding: boolean
  inject_media_generation: boolean
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
  inject_web_search?: boolean
  inject_media_understanding?: boolean
  inject_media_generation?: boolean
  expires_at?: string
  model_ids?: string[]
}
