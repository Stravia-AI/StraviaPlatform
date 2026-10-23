import { describe, expect, test } from 'bun:test'

import { availableSettingsGroups, resolveSettingsGroup, settingsGroupHref } from '../src/lib/settings-groups'

describe('settings groups', () => {
  test('server hosts list general, network, data, account and advanced in task order', () => {
    expect(availableSettingsGroups({ server: true })).toEqual(['general', 'network', 'data', 'account', 'advanced'])
  })

  test('desktop hosts hide the account group instead of disabling it', () => {
    expect(availableSettingsGroups({ server: false })).toEqual(['general', 'network', 'data', 'advanced'])
  })

  test('missing, unknown and unavailable values fall back to general', () => {
    const server = availableSettingsGroups({ server: true })
    const desktop = availableSettingsGroups({ server: false })
    expect(resolveSettingsGroup(null, server)).toBe('general')
    expect(resolveSettingsGroup('', server)).toBe('general')
    expect(resolveSettingsGroup('billing', server)).toBe('general')
    expect(resolveSettingsGroup('account', desktop)).toBe('general')
    expect(resolveSettingsGroup('Network', server)).toBe('general')
  })

  test('available values resolve to themselves', () => {
    const server = availableSettingsGroups({ server: true })
    for (const group of server) expect(resolveSettingsGroup(group, server)).toBe(group)
  })

  test('hrefs link directly to a group with an optional section anchor', () => {
    expect(settingsGroupHref('general')).toBe('settings?view=general')
    expect(settingsGroupHref('network')).toBe('settings?view=network')
    expect(settingsGroupHref('network', 'client')).toBe('settings?view=network#client')
  })
})
