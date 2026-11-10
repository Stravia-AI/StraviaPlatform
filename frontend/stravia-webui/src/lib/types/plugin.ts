// Mirrors stravia-core plugin::{PluginSummary, PluginPreview, ConfirmPluginUpdate}.
export type PluginSource = 'builtin' | 'local' | 'marketplace'

export interface PluginInstallOrigin {
  source_id: string
  repository_id: number
  repository: string
  public_key_fingerprint: string
  trust_revision: number
  release_id: number
  release_tag: string
  release_url: string
  catalog_sha256: string
  artifact_sha256: string
}

export interface MarketplaceError {
  code: string
  message: string
}

export interface PluginSourceSummary {
  id: string
  repository_id: number
  repository: string
  url: string
  official: boolean
  public_key: string
  fingerprint: string
  trust_revision: number
  enabled: boolean
  include_prerelease: boolean
  mirrored: boolean | null
  checked_at: number | null
  succeeded_at: number | null
  error: MarketplaceError | null
}

export interface CatalogRelease {
  vendor_id: string
  version: string
  display_name: string
  description?: string | null
  authors: string[]
  icon_svg?: string | null
  published_at: string
  compatibility: { host_version_req: string; wit_version: string; canonical_format_version: number }
  artifact: { filename: string; compression: 'zstd'; media_type: 'application/zstd'; size: number; sha256: string }
  component: { size: number; sha256: string }
}

export interface SourcePreview {
  preview_id: string
  id: string
  repository_id: number
  repository: string
  url: string
  public_key: string
  fingerprint: string
  mirrored: boolean
  releases: CatalogRelease[]
  previous_fingerprint?: string | null
}

export interface MarketplaceRelease extends CatalogRelease {
  release_id: number
  release_tag: string
  release_url: string
  catalog_sha256: string
  compatible: boolean
  conflict: boolean
  incompatibility: string | null
  prerelease: boolean
}

export interface MarketplaceEntry {
  source_id: string
  vendor_id: string
  releases: MarketplaceRelease[]
  incomplete: boolean
  conflict: boolean
  latest: MarketplaceRelease | null
  update: MarketplaceRelease | null
}

export interface MarketplaceView {
  sources: PluginSourceSummary[]
  entries: MarketplaceEntry[]
  update_count: number
}

export interface MarketplaceInstallInput {
  source_id: string
  release_id: number
  vendor_id: string
  version: string
  catalog_sha256: string
}

export interface MarketplaceProgress {
  stage: 'download' | 'verify' | 'decompress' | 'admission'
  received?: number
  total?: number
}

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
  upstream_model: string
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
  origin: PluginInstallOrigin | null
  previous_origin: PluginInstallOrigin | null
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
  icon_svg?: string | null
}

export interface PluginSummary {
  vendor_id: string
  name: string
  version: string
  source: PluginSource
  origin: PluginInstallOrigin | null
  digest: string
  installed_at: number
  status: string
  error: string | null
  builtin_version: string | null
  capabilities: string[]
  affected_bindings: PluginBindingImpact[]
  pending_update: PluginPreview | null
  icon_svg?: string | null
}

export interface ConfirmPluginUpdate {
  preview_id: string
  allow_data_discard: boolean
}
