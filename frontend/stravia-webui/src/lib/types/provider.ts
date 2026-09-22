import type { ProviderOAuthStatus } from './oauth'
import type { ThinkingLevelMapping } from './route'

export interface Provider {
  id: string
  name: string
  /** Supplier profile type ID (for example, `openai-codex`), not a plugin package ID. */
  vendor?: string | null
  protocol: string
  base_url: string
  use_proxy: boolean
  oauth_status?: ProviderOAuthStatus
  oauth_expires_at?: string | null
  oauth_last_error?: string | null
  oauth_updated_at?: string | null
  preset_key?: string | null
  channel?: string | null
  /** Non-secret values declared by the installed Vendor plugin. */
  vendor_options?: Record<string, unknown>
  /** Descriptor-declared secret keys that have saved values; values are never returned. */
  configured_credential_fields?: string[]
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

export interface ModelSpecification {
  limit?: ProviderModelLimit | null
  modalities?: ProviderModelModalities | null
  reasoning?: boolean | null
  tool_call?: boolean | null
  structured_output?: boolean | null
  attachment?: boolean | null
  temperature?: boolean | null
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

export interface ProviderModelMetadata extends ModelSpecification {
  id?: string | null
  name?: string | null
  description?: string | null
  family?: string | null
  open_weights?: boolean | null
  reasoning_options?: ProviderModelReasoningOption[] | null
  interleaved?: ProviderModelInterleaved | null
  knowledge?: string | null
  release_date?: string | null
  last_updated?: string | null
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
  specification: ModelSpecification
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

export interface CreateProvider {
  name?: string
  source: { type: 'custom'; vendor: string; channel: string; protocol?: string; base_url: string }
  credential:
    | { type: 'api_key'; value: string }
    | { type: 'setup_token'; value: string }
    | { type: 'fields'; values: Record<string, unknown> }
    | { type: 'none' }
  vendor_options?: Record<string, unknown>
  use_proxy?: boolean
}

export type VendorCapability =
  'infer' | 'compact' | 'search' | 'media_image' | 'auth_oauth' | 'model_discovery' | 'allowance' | 'config_validation'

export type VendorAuthCallbackPort = { kind: 'fixed'; primary: number; fallback?: number | null } | { kind: 'dynamic' }

export interface VendorAuthCallback {
  bind_host: string
  redirect_host: string
  path: string
  port: VendorAuthCallbackPort
  manual_redirect_uri?: string | null
  cancel_path?: string | null
}

export interface VendorAuthManualInput {
  type: 'text' | 'callback_url'
  label: string
  description?: string | null
  secret: boolean
}

export interface VendorAuthDescriptor {
  flow: 'authorization_code' | 'device_code' | 'manual'
  callback?: VendorAuthCallback | null
  manual_input?: VendorAuthManualInput | null
}

export interface VendorChannelDescriptor {
  id: string
  name: string
  description?: string | null
  auth?: VendorAuthDescriptor | null
  /** Optional host egress protocol; custom vendor wire protocols may omit it. */
  protocol?: string | null
  /** Initial connection URL proposed by the plugin; saving still requires preview. */
  default_base_url?: string | null
  /** Discovery source selected when a new connection does not provide one. */
  default_models_source?: 'catalog' | null
  capabilities: VendorCapability[]
  model_capabilities?: string[]
  search_model_required: boolean
}

export type VendorConfigFieldKind =
  | { type: 'bool' }
  | { type: 'string'; multiline: boolean }
  | { type: 'int' }
  | { type: 'decimal' }
  | { type: 'enum'; options: Array<{ value: string; label: string }> }

export interface VendorConfigField {
  key: string
  label: string
  description?: string | null
  kind: VendorConfigFieldKind
  required: boolean
  default_json?: unknown
  group?: string | null
  secret: boolean
  min?: number | null
  max?: number | null
  max_length?: number | null
  pattern?: string | null
  visible_when?: { field: string; equals: unknown } | null
}

export interface VendorNetworkDeclaration {
  base_url_field?: string | null
  extra_origins: Array<{ scheme: string; host: string; port?: number | null }>
  field_origins: string[]
}

export interface VendorDataCompatibility {
  config_fields_format: number
  private_state_format: number
  credentials_format: number
  model_metadata_format: number
}

export interface ProviderDescriptor {
  /** Stable supplier profile identity. This is not a saved connection UUID. */
  provider_id: string
  /** Optional catalog identity used for related model metadata and provider branding. */
  catalog_id: string | null
  display_name: string
  description: string | null
  channels: VendorChannelDescriptor[]
  capabilities: VendorCapability[]
  config_fields: VendorConfigField[]
  network: VendorNetworkDeclaration
  data_compat: VendorDataCompatibility
}

export interface ProviderConfigurationPreviewInput {
  /** Saved connection UUID when reviewing an existing connection. */
  provider_id?: string
  /** Supplier profile type ID; the backend field name remains `vendor_id`. */
  vendor_id: string
  channel: string
  base_url: string
  options: Record<string, unknown>
  credentials: Record<string, unknown>
}

export interface ProviderValidationIssue {
  field: string | null
  code: string
  message: string
}

export interface ProviderNetworkPermission {
  origin: string
  configuration_field: string | null
  connection_scoped: boolean
}

export interface ProviderConfigurationPreview {
  base_url: string
  issues: ProviderValidationIssue[]
  network_permissions: ProviderNetworkPermission[]
}

export interface UpdateProvider {
  name?: string
  base_url?: string
  use_proxy?: boolean
  /** Submitted secret keys replace those values; omitted keys keep their saved values. */
  adapter_credentials?: Record<string, unknown>
  /** Replaces all non-secret Vendor configuration values when present. */
  vendor_options?: Record<string, unknown>
  is_enabled?: boolean
}
