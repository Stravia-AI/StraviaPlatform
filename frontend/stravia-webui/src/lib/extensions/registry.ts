export type ExtensionNavSection = 'setup' | 'advanced' | 'monitor' | 'system'

export interface ExtensionNavItem {
  id: string
  capability: string
  section: ExtensionNavSection
  href: string
  label: () => string
  badge?: () => string
  order?: number
}

export interface ExtensionSettingsGroup {
  id: string
  capability: string
  label: () => string
  description?: () => string
  order?: number
}

export interface ExtensionSlotContribution {
  id: string
  slot: string
  capability: string
  order?: number
  title?: () => string
  description?: () => string
  href?: string
}

export interface WebUiExtensionModule {
  id: string
  navItems?: readonly ExtensionNavItem[]
  settingsGroups?: readonly ExtensionSettingsGroup[]
  slots?: readonly ExtensionSlotContribution[]
}

function sortByOrder<T extends { id: string; order?: number }>(items: T[]): T[] {
  return items.sort((a, b) => (a.order ?? 100) - (b.order ?? 100) || a.id.localeCompare(b.id))
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

  hasCapability(capabilities: readonly string[], capability: string): boolean {
    return capabilities.includes(capability)
  }

  activeNavItems(capabilities: readonly string[], section?: ExtensionNavSection): ExtensionNavItem[] {
    const active = new Set(capabilities)
    const items: ExtensionNavItem[] = []
    for (const mod of this.#modules.values()) {
      for (const item of mod.navItems ?? []) {
        if (!active.has(item.capability)) continue
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
        if (!active.has(group.capability)) continue
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
        if (!active.has(contribution.capability)) continue
        contributions.push(contribution)
      }
    }
    return sortByOrder(contributions)
  }
}

export const webUiExtensions = new WebUiExtensionRegistry()
