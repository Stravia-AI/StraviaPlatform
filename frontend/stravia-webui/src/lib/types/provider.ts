import type { ProviderOAuthStatus } from './oauth'
import type { ThinkingLevelMapping } from './route'

export interface Provider {
  id: string
  name: string
  vendor?: string | null
  protocol: string
  base_url: string
  api_key?: string
  use_proxy: boolean
  auth_mode?: 'apikey' | 'oauth'
  oauth_status?: ProviderOAuthStatus
  oauth_expires_at?: string | null
  oauth_last_error?: string | null
  oauth_updated_at?: string | null
  preset_key?: string | null
  channel?: string | null
  models_source?: string | null
  static_models?: string | null
  is_enabled: boolean
  created_at: string
  updated_at: string
}

export interface ImageCapabilityDrift {
  id: string
  provider_id: string
  upstream_model: string
  fingerprint: string
  safe_message: string
  suppressed_until: number
  created_at: number
}

export interface TestResult {
  success: boolean
  latency_ms: number
  model?: string
  error?: string
}

export interface ModelCapabilities {
  provider: string
  model_id: string
  context_window: number
  embedding_length?: number | null
  tool_call: boolean
  reasoning: boolean
  input_modalities: string[]
  output_modalities: string[]
}

export type ProviderProtocol =
  | 'openai-compatible'
  | 'open-responses'
  | 'anthropic-messages'
  | 'google-gemini'
  | 'bedrock-converse'
  | 'cohere-chat'
  | 'watsonx-text-chat'
  | 'gateway-language-model'

export type CatalogAuthMode = 'optional_api_key' | 'oauth' | 'setup_token'

export interface CatalogChannel {
  id: string
  label: string
  protocol: ProviderProtocol
  base_url: string
  auth_mode: CatalogAuthMode
  fingerprint: string
}

export interface CatalogProvider {
  id: string
  name: string
  documentation_url?: string | null
  npm: string
  vendor_id: string
  protocol: ProviderProtocol
  base_url: string
  channels: CatalogChannel[]
}

export interface CatalogProviderList {
  revision: string
  generated_at: string
  providers: CatalogProvider[]
}

export interface CanonicalModelSummary {
  id: string
  name: string
}

export interface CanonicalModelList {
  revision: string
  generated_at: string
  models: CanonicalModelSummary[]
}

export type ProviderModelSourceKind = 'discovered' | 'manual'

export type ProviderModelSelectionPolicy = 'auto' | 'force_enabled' | 'force_disabled'

export type ProviderModelReasoningOption =
  | { type: 'toggle' }
  | { type: 'effort'; values: Array<string | null> }
  | { type: 'budget_tokens'; min?: number | null; max?: number | null }

export type ProviderModelInterleaved = boolean | { field: string }

export interface ProviderModelModalities {
  input: string[]
  output: string[]
}

export interface ProviderModelLimit {
  context?: number | null
  input?: number | null
  output?: number | null
}

export interface ProviderModelPrices {
  input?: number
  output?: number
  reasoning?: number
  cache_read?: number
  cache_write?: number
  input_audio?: number
  output_audio?: number
}

export interface ProviderModelCostTier extends ProviderModelPrices {
  tier: { type: string; size: number }
}

export interface ProviderModelCost extends ProviderModelPrices {
  context_over_200k?: ProviderModelPrices | null
  tiers: ProviderModelCostTier[]
}

export interface ProviderModelMetadata {
  id?: string | null
  name?: string | null
  description?: string | null
  family?: string | null
  attachment?: boolean | null
  reasoning?: boolean | null
  tool_call?: boolean | null
  open_weights?: boolean | null
  reasoning_options?: ProviderModelReasoningOption[] | null
  interleaved?: ProviderModelInterleaved | null
  structured_output?: boolean | null
  temperature?: boolean | null
  knowledge?: string | null
  release_date?: string | null
  last_updated?: string | null
  modalities?: ProviderModelModalities | null
  limit?: ProviderModelLimit | null
  cost?: ProviderModelCost | null
  status?: string | null
  experimental?: unknown
  provider?: unknown
  [key: string]: unknown
}

export interface ProviderModelSummary {
  id: string
  name: string
  available: boolean
  source_kind: ProviderModelSourceKind
  selection_policy: ProviderModelSelectionPolicy
  capabilities: { attachment: boolean; reasoning: boolean; tool_call: boolean; context?: number | null }
  revision: number
}

export interface ProviderModelList {
  models: ProviderModelSummary[]
}

export interface ProviderModelDetail {
  id: string
  available: boolean
  source_kind: ProviderModelSourceKind
  can_reimport: boolean
  selection_policy: ProviderModelSelectionPolicy
  metadata: ProviderModelMetadata
  thinking_level_map?: ThinkingLevelMapping[]
  extensions: Record<string, unknown>
  revision: number
  created_at: string
  updated_at: string
}

export interface PreparedProviderModel {
  id: string
  metadata: ProviderModelMetadata
  extensions: Record<string, unknown>
}

export interface ProviderModelSyncSummary {
  added: number
  missing: number
  restored: number
  deprecated: number
}

export interface CatalogRefreshSummary {
  revision: string
  generated_at: string
  provider_count: number
  model_count: number
  changed: boolean
}

export interface CreateProvider {
  name?: string
  source:
    | { type: 'catalog'; provider_id: string; channel_id: string; fingerprint: string; base_url_override?: string }
    | {
        type: 'custom'
        vendor?: string
        protocol: string
        base_url: string
        models_source?: string
        static_models?: string
      }
  credential:
    | { type: 'api_key'; value: string }
    | { type: 'setup_token'; value: string }
    | { type: 'fields'; values: Record<string, string> }
    | { type: 'none' }
  use_proxy?: boolean
}

export interface VendorCredentialField {
  key: string
  label: string
  secret: boolean
  required: boolean
  input: 'text' | 'password' | 'textarea'
}

export interface VendorMetadata {
  id: string
  label: { zh: string; en: string }
  icon: string
  defaultProtocol: ProviderProtocol
  credentialFields: VendorCredentialField[]
}

export interface UpdateProvider {
  name?: string
  vendor?: string
  protocol?: string
  base_url?: string
  use_proxy?: boolean
  auth_mode?: 'apikey' | 'oauth'
  preset_key?: string
  channel?: string
  models_source?: string
  static_models?: string
  api_key?: string
  is_enabled?: boolean
}
