import type { ThinkingLevel } from './route'

export type WebProviderKind = 'local' | 'exa' | 'zhipu'
export type LocalSearchEngineId = 'google' | 'bing' | 'brave' | 'baidu' | '360' | 'sogou_weixin' | 'google_scholar'

export interface LocalSearchEngineConfig {
  enabled: boolean
}

export type LocalSearchEngineConfigs = Record<LocalSearchEngineId, LocalSearchEngineConfig>

export interface WebProvider {
  id: string
  name: string
  kind: WebProviderKind
  use_proxy: boolean
  local_engines?: LocalSearchEngineConfigs | null
  capabilities: { search: boolean; fetch: boolean }
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
  local_engines?: LocalSearchEngineConfigs | null
}

export interface UpdateWebProvider {
  name?: string
  api_key?: string
  use_proxy?: boolean
  local_engines?: LocalSearchEngineConfigs
}

export interface WebAccessSettings {
  enabled: boolean
  search_provider_ids: string[]
  fetch_provider_ids: string[]
}

export type WebSearchBackend =
  | { kind: 'local'; model_id?: string | null }
  | { kind: 'codex'; provider_id?: string | null; upstream_model?: string | null }

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

export interface CompatibleCodexProvider {
  id: string
  name: string
  models: { id: string }[]
}

export type MediaUnderstandingState = 'disabled' | 'unavailable' | 'available'

export interface MediaUnderstandingConfigView {
  enabled: boolean
  model_id?: string | null
  thinking_level?: ThinkingLevel | null
  state: MediaUnderstandingState
  eligible_models: EligibleMediaModel[]
}

export interface UpdateMediaUnderstandingConfig {
  enabled: boolean
  model_id?: string | null
  thinking_level?: ThinkingLevel | null
}
