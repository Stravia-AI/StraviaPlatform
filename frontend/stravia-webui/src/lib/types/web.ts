import type { ThinkingLevel } from './route'

export type WebProviderKind = string

export interface WebProviderCapabilities {
  search: boolean
  fetch: boolean
}

export interface WebProviderKindDescriptor {
  kind: WebProviderKind
  capabilities: WebProviderCapabilities
  requires_api_key: boolean
  singleton: boolean
  allow_delete: boolean
}

export interface WebProvider {
  id: string
  name: string
  kind: WebProviderKind
  api_key_configured: boolean
  use_proxy: boolean
  config?: Record<string, unknown> | null
  capabilities: WebProviderCapabilities | null
  registered: boolean
  available: boolean
  unavailable_reason?: string | null
  allow_delete: boolean
  last_test_success?: boolean | null
  last_test_at?: string | null
  created_at: string
  updated_at: string
}

export interface CreateWebProvider {
  name: string
  kind: WebProviderKind
  api_key?: string | null
  use_proxy?: boolean
  config?: Record<string, unknown> | null
}

export interface UpdateWebProvider {
  name?: string
  api_key?: string | null
  use_proxy?: boolean
  config?: Record<string, unknown> | null
}

export interface WebAccessSettings {
  search_provider_ids: string[]
  fetch_provider_ids: string[]
}

export type WebSearchBackend =
  { kind: 'local'; model_id?: string | null } | { kind: 'external'; route_id?: string | null }

export interface WebSearchConfig {
  revision: number
  enabled: boolean
  backend?: WebSearchBackend | null
  max_turns: number
  total_time_seconds: number
  updated_at: string
}

export type UpdateWebSearchConfig = WebSearchConfig

export interface WebSearchLimits {
  min_turns: number
  max_turns: number
  min_total_time_seconds: number
  max_total_time_seconds: number
}

export interface WebSearchConfigView extends WebSearchConfig {
  limits: WebSearchLimits
}

export interface EligibleSearchModel {
  id: string
  model_id: string
  display_name: string
}

export interface EligibleMediaModel {
  id: string
  model_id: string
  display_name: string
  supported_thinking_levels: ThinkingLevel[]
}

export interface ExternalSearchRoute {
  id: string
  model_id: string
  display_name: string
  available: boolean
}

export type MediaUnderstandingState = 'disabled' | 'unavailable' | 'available'

export type MediaUnderstandingKind = 'image' | 'audio'

export interface MediaUnderstandingKindConfig {
  enabled: boolean
  model_id?: string | null
  thinking_level?: ThinkingLevel | null
  state: MediaUnderstandingState
  eligible_models: EligibleMediaModel[]
}

export type MediaUnderstandingConfigView = Record<MediaUnderstandingKind, MediaUnderstandingKindConfig>

export interface UpdateMediaUnderstandingConfig {
  enabled: boolean
  model_id?: string | null
  thinking_level?: ThinkingLevel | null
}
