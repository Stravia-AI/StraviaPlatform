import type { Component } from 'svelte'

export type ExtensionNavSection = 'setup' | 'advanced' | 'monitor' | 'system'

export type ExtensionCapabilityRequirement = string | readonly string[]

export interface ExtensionNavItem {
  id: string
  capability: ExtensionCapabilityRequirement
  section: ExtensionNavSection
  href: string
  label: () => string
  badge?: () => string
  order?: number
}

export interface ExtensionSettingsGroup {
  id: string
  capability: ExtensionCapabilityRequirement
  label: () => string
  description?: () => string
  order?: number
}

export interface ExtensionSlotContribution {
  id: string
  slot: string
  capability: ExtensionCapabilityRequirement
  order?: number
  title?: () => string
  description?: () => string
  href?: string
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  component?: () => Promise<{ default: Component<any> }>
}

export interface ExtensionRoute {
  id: string
  /** Path relative to `/ext`, e.g. `/reports` or `/reports/:id`. */
  pattern: string
  capability: ExtensionCapabilityRequirement
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  load: () => Promise<{ default: Component<any> }>
  breadcrumbLabel?: () => string
}

export interface MatchedExtensionRoute {
  route: ExtensionRoute
  params: Record<string, string>
}

export interface WebUiExtensionModule {
  id: string
  navItems?: readonly ExtensionNavItem[]
  settingsGroups?: readonly ExtensionSettingsGroup[]
  slots?: readonly ExtensionSlotContribution[]
  routes?: readonly ExtensionRoute[]
}

function sortByOrder<T extends { id: string; order?: number }>(items: T[]): T[] {
  return items.sort((a, b) => (a.order ?? 100) - (b.order ?? 100) || a.id.localeCompare(b.id))
}

function matchesCapability(active: ReadonlySet<string>, requirement: ExtensionCapabilityRequirement): boolean {
  if (typeof requirement === 'string') {
    return active.has(requirement)
  }
  return requirement.some((cap) => active.has(cap))
}

function splitPathSegments(raw: string): string[] {
  const withoutQuery = raw.split(/[?#]/, 1)[0] ?? ''
  return withoutQuery.split('/').filter((segment) => segment.length > 0)
}

function decodeSegment(segment: string): string {
  try {
    return decodeURIComponent(segment)
  } catch {
    return segment
  }
}

function matchPattern(pattern: string, path: string): { params: Record<string, string>; staticCount: number } | null {
  const patternSegments = splitPathSegments(pattern)
  const pathSegments = splitPathSegments(path)
  if (patternSegments.length !== pathSegments.length) return null

  const params: Record<string, string> = {}
  let staticCount = 0

  for (let i = 0; i < patternSegments.length; i += 1) {
    const patternSeg = patternSegments[i]
    const pathSeg = pathSegments[i]
    if (patternSeg.startsWith(':') && patternSeg.length > 1) {
      params[patternSeg.slice(1)] = decodeSegment(pathSeg)
    } else if (patternSeg === pathSeg) {
      staticCount += 1
    } else {
      return null
    }
  }

  return { params, staticCount }
}

/**
 * Data-driven WebUI extension registry gated by `/api/v1/capabilities`.
 *
 * Community Edition builds register zero modules and never request commercial endpoints.
 * Commercial builds inject extension modules at build time via `#webui-extensions`.
 */
export class WebUiExtensionRegistry {
  #modules = new Map<string, WebUiExtensionModule>()

  register(module: WebUiExtensionModule): void {
    this.#modules.set(module.id, module)
  }

  unregister(id: string): void {
    this.#modules.delete(id)
  }

  clear(): void {
    this.#modules.clear()
  }

  listModuleIds(): string[] {
    return [...this.#modules.keys()].sort()
  }

  hasCapability(capabilities: readonly string[], capability: ExtensionCapabilityRequirement): boolean {
    if (typeof capability === 'string') {
      return capabilities.includes(capability)
    }
    const active = new Set(capabilities)
    return capability.some((cap) => active.has(cap))
  }

  activeNavItems(capabilities: readonly string[], section?: ExtensionNavSection): ExtensionNavItem[] {
    const active = new Set(capabilities)
    const items: ExtensionNavItem[] = []
    for (const mod of this.#modules.values()) {
      for (const item of mod.navItems ?? []) {
        if (!matchesCapability(active, item.capability)) continue
        if (section && item.section !== section) continue
        items.push(item)
      }
    }
    return sortByOrder(items)
  }

  activeSettingsGroups(capabilities: readonly string[]): ExtensionSettingsGroup[] {
    const active = new Set(capabilities)
    const groups: ExtensionSettingsGroup[] = []
    for (const mod of this.#modules.values()) {
      for (const group of mod.settingsGroups ?? []) {
        if (!matchesCapability(active, group.capability)) continue
        groups.push(group)
      }
    }
    return sortByOrder(groups)
  }

  activeSlots(capabilities: readonly string[], slot: string): ExtensionSlotContribution[] {
    const active = new Set(capabilities)
    const contributions: ExtensionSlotContribution[] = []
    for (const mod of this.#modules.values()) {
      for (const contribution of mod.slots ?? []) {
        if (contribution.slot !== slot) continue
        if (!matchesCapability(active, contribution.capability)) continue
        contributions.push(contribution)
      }
    }
    return sortByOrder(contributions)
  }

  matchRoute(capabilities: readonly string[], path: string): MatchedExtensionRoute | null {
    const active = new Set(capabilities)
    const candidates: Array<{ route: ExtensionRoute; params: Record<string, string>; staticCount: number }> = []
    for (const mod of this.#modules.values()) {
      for (const route of mod.routes ?? []) {
        if (!matchesCapability(active, route.capability)) continue
        const matched = matchPattern(route.pattern, path)
        if (!matched) continue
        candidates.push({ route, params: matched.params, staticCount: matched.staticCount })
      }
    }
    if (candidates.length === 0) return null
    candidates.sort((a, b) => b.staticCount - a.staticCount || a.route.id.localeCompare(b.route.id))
    const winner = candidates[0]
    return { route: winner.route, params: winner.params }
  }
}

export const webUiExtensions = new WebUiExtensionRegistry()
