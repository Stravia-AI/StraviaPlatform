import { describe, expect, test } from 'bun:test'
import { ensureBuildExtensionsRegistered, WebUiExtensionRegistry } from '#lib/extensions/index.js'

describe('WebUiExtensionRegistry', () => {
  test('CE build entry registers zero modules', () => {
    const registry = ensureBuildExtensionsRegistered()
    expect(registry.listModuleIds()).toEqual([])
    expect(registry.activeNavItems(['license_status'])).toEqual([])
    expect(registry.activeSettingsGroups(['license_status'])).toEqual([])
  })

  test('filters nav items, settings groups, and slots by active capabilities and orders deterministically', () => {
    const registry = new WebUiExtensionRegistry()
    registry.register({
      id: 'commercial-license',
      navItems: [
        {
          id: 'ee-audit',
          capability: 'audit_log',
          section: 'monitor',
          href: '/audit',
          label: () => 'Audit Log',
          order: 20,
        },
        {
          id: 'ee-license',
          capability: 'license_status',
          section: 'system',
          href: '/settings?view=license',
          label: () => 'License',
          order: 10,
        },
      ],
      settingsGroups: [{ id: 'license', capability: 'license_status', label: () => 'License', order: 50 }],
      slots: [
        {
          id: 'settings-license-banner',
          slot: 'settings:general:top',
          capability: 'license_status',
          title: () => 'Commercial License',
          order: 10,
        },
      ],
    })

    // CE capabilities: nothing active
    expect(registry.activeNavItems([])).toEqual([])
    expect(registry.activeSettingsGroups([])).toEqual([])
    expect(registry.activeSlots([], 'settings:general:top')).toEqual([])

    // Commercial with only license_status enabled
    expect(registry.activeNavItems(['license_status']).map((i) => i.id)).toEqual(['ee-license'])
    expect(registry.activeNavItems(['license_status'], 'monitor')).toEqual([])
    expect(registry.activeNavItems(['license_status'], 'system').map((i) => i.id)).toEqual(['ee-license'])
    expect(registry.activeSettingsGroups(['license_status']).map((g) => g.id)).toEqual(['license'])
    expect(registry.activeSlots(['license_status'], 'settings:general:top').map((s) => s.id)).toEqual([
      'settings-license-banner',
    ])

    // Unregister removes module cleanly
    registry.unregister('commercial-license')
    expect(registry.listModuleIds()).toEqual([])
  })
})
