import type { ProviderCostPlan } from './money'
import type { ProviderOAuthStatus } from './oauth'
import type { RouteOutputModality, ThinkingLevelMapping } from './route'

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
  /** ADR-0073: `ok` | `invalid` — upstream confirmed rejection of the current credentials. */
  credential_status?: string
  credential_invalid_at?: string | null
  allowance_suspension?: import('./provider-allowance').AllowanceSuspension | null
  /** Current effective cost plan; null means usage-based (default). */
  cost_plan?: ProviderCostPlan | null
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

/** What a connection check actually confirmed; never widen this to imply more. */
export type ConnectionCheckVerification = 'endpoint' | 'credentials' | 'endpoint_and_credentials'

export type ConnectionCheckStage = 'configuration' | 'transport' | 'upstream' | 'response'

export interface ProviderConnectionCheckPassed {
  status: 'passed'
  verified: ConnectionCheckVerification
  duration_ms: number
}

export interface ProviderConnectionCheckFailed {
  status: 'failed'
  stage: ConnectionCheckStage
  code: string
  field?: string | null
  upstream_status?: number | null
  issues?: ProviderValidationIssue[]
}

export interface ProviderConnectionCheckUnsupported {
  status: 'unsupported'
  code: string
}

export type ProviderConnectionCheckResult =
  ProviderConnectionCheckPassed | ProviderConnectionCheckFailed | ProviderConnectionCheckUnsupported

/**
 * Draft check request. `configuration` carries the current form values;
 * omitted secret fields keep their saved values server-side. A ready OAuth
 * session is referenced by `authorization_id` and must not be bound.
 */
export interface ProviderConnectionCheckInput {
  configuration?: ProviderConfigurationPreviewInput
  use_proxy?: boolean
  authorization_id?: string
}

export interface ModelCapabilities {
  provider: string
  model_id: string
  context_window: number
  embedding_length?: number | null
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

export type SourceStamp =
  { type: 'provider_catalog'; provider_id: string } | { type: 'canonical'; model_id: string } | { type: 'discovery' }

export type SnapshotState =
  | { type: 'unregistered' }
  | { type: 'imported'; source: SourceStamp; pinned: string[] }
  | { type: 'edited'; source: SourceStamp | null }

export type ProviderModelSelectionPolicy = 'auto' | 'force_enabled' | 'force_disabled'

export interface ProviderModelModalities {
  input: string[]
  output: string[]
}

export interface ProviderModelLimit {
  context?: number | null
}

export interface ModelSpecification {
  limit?: ProviderModelLimit | null
  modalities?: ProviderModelModalities | null
  reasoning_efforts?: string[]
}

export interface ProviderModelPrices {
  /** Decimal string in API responses and writes; numbers are tolerated for legacy/source catalog input. */
  input?: number | string | null
  output?: number | string | null
  cache_read?: number | string | null
  cache_write?: number | string | null
}

export interface ProviderModelCostTier extends ProviderModelPrices {
  tier: { type: string; size: number }
}

/** Price fields that can be recorded in the custom currency. `tiers` covers every tier, including `context_over_200k`. */
export type PriceFieldName = 'input' | 'output' | 'cache_read' | 'cache_write' | 'tiers'

export interface PeakPricingWindow {
  /** `HH:MM`; start is inclusive. */
  start: string
  /** `HH:MM` or `24:00`; end is exclusive. Windows never cross midnight. */
  end: string
}

export interface PeakPricing {
  timezone: string
  /** ISO weekdays, Monday = 1. */
  weekdays: number[]
  ranges: PeakPricingWindow[]
  /** Decimal string. */
  multiplier: string | number
}

export interface ProviderModelCost extends ProviderModelPrices {
  context_over_200k?: ProviderModelPrices | null
  tiers: ProviderModelCostTier[]
  custom_currency_fields?: PriceFieldName[]
  peak?: PeakPricing | null
}

export interface ProviderModelMetadata extends ModelSpecification {
  id?: string | null
  name?: string | null
  description?: string | null
  family?: string | null
  open_weights?: boolean | null
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
  route_output_modalities: RouteOutputModality[]
  name: string
  available: boolean
  source_kind: ProviderModelSourceKind
  snapshot_state: SnapshotState
  selection_policy: ProviderModelSelectionPolicy
  specification: ModelSpecification
  revision: number
}

export interface ProviderModelList {
  models: ProviderModelSummary[]
}

export interface ProviderModelDetail {
  id: string
  route_output_modalities: RouteOutputModality[]
  available: boolean
  source_kind: ProviderModelSourceKind
  snapshot_state: SnapshotState
  pinned_fields: string[]
  restorable_fields: string[]
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
  snapshot_state: SnapshotState
  metadata: ProviderModelMetadata
  extensions: Record<string, unknown>
}

export interface ProviderModelSyncSummary {
  checked_at: string
  source: ModelChangeSource
  record_id: string | null
  added: number
  missing: number
  restored: number
  deprecated: number
  deprecation_cleared: number
  specification_updated: number
  outcome: ModelChangeOutcome
  error: string | null
}

export type ModelChangeSource = 'automatic_sync' | 'manual_sync' | 'manual_change'
export type ModelChangeOutcome = 'success' | 'partial_success' | 'failure'
export type ModelChangeType =
  | 'added'
  | 'missing'
  | 'restored'
  | 'deprecated'
  | 'deprecation_cleared'
  | 'specification_updated'
  | 'manual_added'
  | 'selection_policy_changed'
  | 'sync_failed'
export interface ModelSpecificationDiff {
  field: string
  before: unknown
  after: unknown
}
export interface ModelChangeItem {
  model_id: string
  name: string
  types: ModelChangeType[]
  specification: ModelSpecificationDiff[]
  before_presence: 'present' | 'missing' | null
  after_presence: 'present' | 'missing' | null
  before_deprecated: boolean | null
  after_deprecated: boolean | null
  before_policy: ProviderModelSelectionPolicy | null
  after_policy: ProviderModelSelectionPolicy | null
}
export interface ModelChangeRecord {
  id: string
  provider_id: string
  provider_name: string
  provider_deleted: boolean
  source: ModelChangeSource
  checked_at: string
  outcome: ModelChangeOutcome
  initial_sync: boolean
  changes: ModelChangeItem[]
  error: string | null
}
export interface ModelChangeQuery {
  provider_id?: string
  type?: ModelChangeType
  source?: ModelChangeSource
  q?: string
  cursor?: string
  limit?: number
}
export interface ModelChangePage {
  records: ModelChangeRecord[]
  next_cursor: string | null
}
export interface ProviderModelSyncState {
  automatic_sync: boolean
  in_progress: boolean
  pause_reason: string | null
  last_check: ProviderModelSyncSummary | null
  has_successful_sync: boolean
}
export interface UpdateProviderModelSyncState {
  automatic_sync: boolean
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

export type LocalizedText = { 'en-US': string; [locale: string]: string }

export interface VendorAuthManualInput {
  type: 'text' | 'callback_url'
  label: LocalizedText
  description?: LocalizedText | null
  secret: boolean
}

export interface VendorAuthDescriptor {
  flow: 'authorization_code' | 'device_code' | 'manual'
  callback?: VendorAuthCallback | null
  manual_input?: VendorAuthManualInput | null
}

export interface VendorChannelDescriptor {
  id: string
  name: LocalizedText
  description?: LocalizedText | null
  auth?: VendorAuthDescriptor | null
  /** Optional host egress protocol; custom vendor wire protocols may omit it. */
  protocol?: string | null
  /** Selectable egress protocols when the channel offers more than one wire protocol. */
  protocols?: Array<{ value: string; label: LocalizedText }>
  /** Initial connection URL proposed by the plugin; saving still requires preview. */
  default_base_url?: string | null
  /** Discovery source selected when a new connection does not provide one. */
  default_models_source?: 'catalog' | null
  /** Whether catalog-sourced discovery consumes the host-injected catalog scope. */
  consumes_catalog_models?: boolean
  capabilities: VendorCapability[]
  model_capabilities?: string[]
}

export type VendorConfigFieldKind =
  | { type: 'bool' }
  | { type: 'string'; multiline: boolean }
  | { type: 'int' }
  | { type: 'decimal' }
  | { type: 'enum'; options: Array<{ value: string; label: LocalizedText }> }

export interface VendorConfigField {
  key: string
  label: LocalizedText
  description?: LocalizedText | null
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
  /** Standalone SVG returned by the host icon endpoint; never injected into the DOM. */
  icon_svg?: string
  display_name: string
  description: string | null
  channels: VendorChannelDescriptor[]
  capabilities: VendorCapability[]
  config_fields: VendorConfigField[]
  config_groups: Array<{ id: string; label: LocalizedText }>
  network: VendorNetworkDeclaration
  data_compat: VendorDataCompatibility
}

/** Management DTO for a descriptor actually supplied by an installed plugin. */
export interface ProviderMetadata extends ProviderDescriptor {
  /** Loaded vendor package identity resolved by the host; profile self-reporting is ignored. */
  plugin: { id: string; name: string }
}

export interface ProviderConfigurationPreviewInput {
  /** Saved connection UUID when reviewing an existing connection. */
  provider_id?: string
  /** Supplier profile type ID; the backend field name remains `vendor_id`. */
  vendor_id: string
  channel: string
  protocol?: string
  base_url: string
  options: Record<string, unknown>
  credentials: Record<string, unknown>
}

export interface ProviderValidationIssue {
  field: string | null
  code: string
  message: LocalizedText
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
