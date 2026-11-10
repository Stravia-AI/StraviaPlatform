import { expect, test } from 'bun:test'

import { marketplaceStatus } from '../src/lib/marketplace-status'
import type { MarketplaceEntry, PluginSourceSummary, PluginSummary } from '../src/lib/types/plugin'

const release = { compatible: true, conflict: false } as MarketplaceEntry['releases'][number]
const incompatible = { compatible: false, conflict: false } as MarketplaceEntry['releases'][number]
const conflicting = { compatible: true, conflict: true } as MarketplaceEntry['releases'][number]
const entry = {
  source_id: 'one',
  releases: [release],
  latest: release,
  update: null,
  conflict: false,
} as MarketplaceEntry
const source = { id: 'one', enabled: true, error: null } as PluginSourceSummary
const installed = { source: 'marketplace', origin: { source_id: 'one' } } as PluginSummary

test('installed-only plugins are not advertised as available downloads', () => {
  expect(marketplaceStatus(undefined, undefined, { ...installed, source: 'builtin', origin: null })).toBe('installed')
})

test('source errors take precedence over available updates', () => {
  expect(
    marketplaceStatus(
      { ...entry, conflict: true, update: release },
      { ...source, error: { code: 'network', message: 'offline' } },
      installed,
    ),
  ).toBe('source-error')
})

test('conflicts block only their version and disabled sources never claim to be latest', () => {
  expect(
    marketplaceStatus(
      { ...entry, conflict: true, releases: [conflicting, release], update: release },
      source,
      installed,
    ),
  ).toBe('update')
  expect(marketplaceStatus({ ...entry, conflict: true, releases: [conflicting, release] }, source, installed)).toBe(
    'latest',
  )
  expect(
    marketplaceStatus({ ...entry, conflict: true, releases: [conflicting], latest: null }, source, installed),
  ).toBe('conflict')
  expect(marketplaceStatus(entry, { ...source, enabled: false }, installed)).toBe('source-disabled')
  expect(
    marketplaceStatus(entry, { ...source, enabled: false, error: { code: 'network', message: 'offline' } }, installed),
  ).toBe('source-disabled')
  expect(marketplaceStatus(undefined, { ...source, enabled: false }, installed)).toBe('source-disabled')
})

test('an update only belongs to its installed source', () => {
  expect(marketplaceStatus({ ...entry, update: release }, source, installed)).toBe('update')
  expect(
    marketplaceStatus({ ...entry, update: release }, source, {
      ...installed,
      origin: { ...installed.origin!, source_id: 'other' },
    }),
  ).toBe('available')
  expect(marketplaceStatus(entry, source, installed)).toBe('latest')
})

test('removed sources and incompatible newest releases remain visible', () => {
  expect(marketplaceStatus(undefined, undefined, installed)).toBe('source-removed')
  expect(marketplaceStatus({ ...entry, releases: [incompatible, release] }, source, installed)).toBe('host-upgrade')
  expect(marketplaceStatus({ ...entry, releases: [incompatible, release], update: release }, source, installed)).toBe(
    'update',
  )
  expect(marketplaceStatus({ ...entry, releases: [], latest: null }, source, installed)).toBe('available')
  expect(marketplaceStatus(entry, source, { ...installed, source: 'local', origin: null })).toBe('available')
})
