import { describe, expect, test } from 'bun:test'
import type { Component } from 'svelte'
import { ensureBuildExtensionsRegistered, WebUiExtensionRegistry } from '#lib/extensions/index.js'

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const dummyComponent = (() => null) as unknown as Component<any>

describe('WebUiExtensionRegistry', () => {
  test('CE build entry registers zero modules', () => {
    const registry = ensureBuildExtensionsRegistered()
    expect(registry.listModuleIds()).toEqual([])
    expect(registry.activeNavItems(['license_status'])).toEqual([])
    expect(registry.activeSettingsGroups(['license_status'])).toEqual([])
    expect(registry.matchRoute(['audit_log', 'compliance_export'], '/reports')).toBeNull()
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

  test('supports array capability matching (ANY active capability matches)', () => {
    const registry = new WebUiExtensionRegistry()
    registry.register({
      id: 'commercial-reports',
      navItems: [
        {
          id: 'reports-hub',
          capability: ['audit_log', 'compliance_export'],
          section: 'advanced',
          href: '/ext/reports',
          label: () => 'Reports',
          order: 10,
        },
      ],
      settingsGroups: [
        { id: 'reports-settings', capability: ['audit_log', 'compliance_export'], label: () => 'Report Settings' },
      ],
      slots: [
        {
          id: 'reports-panel',
          slot: 'api-key:policy',
          capability: ['audit_log', 'compliance_export'],
          component: () => Promise.resolve({ default: dummyComponent }),
          order: 10,
        },
      ],
    })

    expect(registry.hasCapability(['audit_log'], ['audit_log', 'compliance_export'])).toBe(true)
    expect(registry.hasCapability(['compliance_export'], ['audit_log', 'compliance_export'])).toBe(true)
    expect(registry.hasCapability(['other'], ['audit_log', 'compliance_export'])).toBe(false)
    expect(registry.hasCapability([], ['audit_log', 'compliance_export'])).toBe(false)

    expect(registry.activeNavItems([])).toEqual([])
    expect(registry.activeNavItems(['audit_log'], 'advanced').map((i) => i.id)).toEqual(['reports-hub'])
    expect(registry.activeNavItems(['compliance_export'], 'advanced').map((i) => i.id)).toEqual(['reports-hub'])
    expect(registry.activeSettingsGroups(['compliance_export']).map((g) => g.id)).toEqual(['reports-settings'])

    const slots = registry.activeSlots(['audit_log'], 'api-key:policy')
    expect(slots.map((s) => s.id)).toEqual(['reports-panel'])
    expect(typeof slots[0]?.component).toBe('function')
  })

  test('matches exact and parameterized routes with capability gating and static precedence', () => {
    const registry = new WebUiExtensionRegistry()
    registry.register({
      id: 'commercial-reports',
      routes: [
        {
          id: 'reports-overview',
          pattern: '/reports',
          capability: ['audit_log', 'compliance_export'],
          load: () => Promise.resolve({ default: dummyComponent }),
          breadcrumbLabel: () => 'Reports',
        },
        {
          id: 'reports-item-detail',
          pattern: '/reports/items/:id',
          capability: ['audit_log', 'compliance_export'],
          load: () => Promise.resolve({ default: dummyComponent }),
          breadcrumbLabel: () => 'Report details',
        },
        {
          id: 'reports-item-new',
          pattern: '/reports/items/new',
          capability: ['audit_log', 'compliance_export'],
          load: () => Promise.resolve({ default: dummyComponent }),
        },
        {
          id: 'reports-group-detail',
          pattern: '/reports/groups/:id',
          capability: ['audit_log', 'compliance_export'],
          load: () => Promise.resolve({ default: dummyComponent }),
          breadcrumbLabel: () => 'Group details',
        },
      ],
    })

    // Unlicensed rejected
    expect(registry.matchRoute([], '/reports')).toBeNull()
    expect(registry.matchRoute(['unrelated_cap'], '/reports/items/item-1')).toBeNull()

    // Exact match
    const overview = registry.matchRoute(['audit_log'], '/reports')
    expect(overview?.route.id).toBe('reports-overview')
    expect(overview?.params).toEqual({})

    // Parameterized match with URL decoding
    const itemDetail = registry.matchRoute(['compliance_export'], '/reports/items/item%201')
    expect(itemDetail?.route.id).toBe('reports-item-detail')
    expect(itemDetail?.params).toEqual({ id: 'item 1' })

    const groupDetail = registry.matchRoute(['audit_log'], '/reports/groups/grp-42')
    expect(groupDetail?.route.id).toBe('reports-group-detail')
    expect(groupDetail?.params).toEqual({ id: 'grp-42' })

    // Static segment takes precedence over `:id`
    const itemNew = registry.matchRoute(['audit_log'], '/reports/items/new')
    expect(itemNew?.route.id).toBe('reports-item-new')
    expect(itemNew?.params).toEqual({})

    // Unknown path returns null
    expect(registry.matchRoute(['audit_log'], '/reports/unknown/path')).toBeNull()
  })
})
