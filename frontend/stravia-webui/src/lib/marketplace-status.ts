import type { MarketplaceEntry, PluginSourceSummary, PluginSummary } from '#lib/types.js'

export type MarketplaceStatus =
  | 'conflict'
  | 'source-error'
  | 'source-disabled'
  | 'source-removed'
  | 'update'
  | 'host-upgrade'
  | 'latest'
  | 'installed'
  | 'available'

/** Present the server's version and compatibility decisions without recomputing them. */
export function marketplaceStatus(
  entry: MarketplaceEntry | undefined,
  source: PluginSourceSummary | undefined,
  installed: PluginSummary | undefined,
): MarketplaceStatus {
  const attached = installed?.source === 'marketplace' && installed.origin?.source_id === entry?.source_id
  if (installed?.source === 'marketplace' && !source && (!entry || attached)) return 'source-removed'
  if (source && !source.enabled) return 'source-disabled'
  if (source?.error) return 'source-error'
  if (attached && entry?.update) return 'update'
  if (entry?.releases[0] && !entry.releases[0].conflict && !entry.releases[0].compatible) return 'host-upgrade'
  if (entry?.latest) return attached ? 'latest' : 'available'
  if (entry?.releases[0]?.conflict) return 'conflict'
  return installed && !entry ? 'installed' : 'available'
}
