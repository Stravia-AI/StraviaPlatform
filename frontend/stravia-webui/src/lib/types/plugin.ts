// Mirrors stravia-core plugin::{PluginSummary, PluginPreview, ConfirmPluginUpdate}.
export type PluginSource = 'builtin' | 'local'

export interface PluginProvider {
  id: string
  name: string
}

export interface PluginNetworkPermission {
  origin: string
  provider_id: string | null
  configuration_field: string | null
  added: boolean
}

export interface PluginDataDiscard {
  provider: PluginProvider
  kinds: string[]
  recovery_actions: string[]
}

export interface PluginBindingImpact {
  route_id: string
  provider_id: string
  upstream_model: string | null
  capability: string
}

export interface PluginPreview {
  id: string
  vendor_id: string
  name: string
  author: string | null
  previous_version: string | null
  new_version: string
  target_source: PluginSource
  is_downgrade: boolean
  inherits_credentials: boolean
  affected_providers: PluginProvider[]
  network_permissions: PluginNetworkPermission[]
  removed_network_permissions: string[]
  discarded_data: PluginDataDiscard[]
  affected_bindings: PluginBindingImpact[]
  cancels_active_operations: boolean
  active_operations: number
  affected_auth_sessions: number
}

export interface PluginSummary {
  vendor_id: string
  name: string
  version: string
  source: PluginSource
  status: string
  error: string | null
  builtin_version: string | null
  capabilities: string[]
  affected_bindings: PluginBindingImpact[]
  pending_update: PluginPreview | null
}

export interface ConfirmPluginUpdate {
  preview_id: string
  allow_data_discard: boolean
}
