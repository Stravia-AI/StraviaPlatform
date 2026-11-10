import { expect, test, type Page } from '@playwright/test'

import type {
  MarketplaceRelease,
  MarketplaceView,
  PluginPreview,
  PluginSourceSummary,
  PluginSummary,
} from '../src/lib/types'
import { prepareApp } from './prepare-app'

const source: PluginSourceSummary = {
  id: 'community',
  repository_id: 42,
  repository: 'example/plugins',
  url: 'https://github.com/example/plugins',
  official: false,
  public_key: 'public-key',
  fingerprint: 'abcdef',
  trust_revision: 1,
  enabled: true,
  include_prerelease: false,
  mirrored: false,
  checked_at: 1700000000,
  succeeded_at: 1700000000,
  error: null,
}
const release: MarketplaceRelease = {
  vendor_id: 'workbuddy',
  version: '2.0.0',
  display_name: 'WorkBuddy',
  description: 'Community plugin',
  authors: ['Example'],
  published_at: '2026-01-01T00:00:00Z',
  compatibility: { host_version_req: '>=1.0.0', wit_version: '1.0.0', canonical_format_version: 1 },
  artifact: {
    filename: 'workbuddy.wasm.zst',
    compression: 'zstd',
    media_type: 'application/zstd',
    size: 1024,
    sha256: 'a'.repeat(64),
  },
  component: { size: 2048, sha256: 'b'.repeat(64) },
  release_id: 10,
  release_tag: 'v2.0.0',
  release_url: 'https://github.com/example/plugins/releases/tag/v2.0.0',
  catalog_sha256: 'c'.repeat(64),
  compatible: true,
  conflict: false,
  incompatibility: null,
  prerelease: false,
}
const origin = {
  source_id: source.id,
  repository_id: source.repository_id,
  repository: source.repository,
  public_key_fingerprint: source.fingerprint,
  trust_revision: 1,
  release_id: 10,
  release_tag: release.release_tag,
  release_url: release.release_url,
  catalog_sha256: release.catalog_sha256,
  artifact_sha256: release.artifact.sha256,
}
const preview: PluginPreview = {
  id: 'preview-1',
  vendor_id: 'workbuddy',
  name: 'WorkBuddy',
  author: 'Example',
  previous_version: null,
  new_version: '2.0.0',
  target_source: 'marketplace',
  origin,
  previous_origin: null,
  is_downgrade: false,
  inherits_credentials: false,
  affected_providers: [],
  network_permissions: [],
  removed_network_permissions: [],
  discarded_data: [],
  affected_bindings: [],
  cancels_active_operations: false,
  active_operations: 0,
  affected_auth_sessions: 0,
}
const installed: PluginSummary = {
  vendor_id: 'workbuddy',
  name: 'WorkBuddy',
  version: '1.0.0',
  source: 'marketplace',
  origin,
  digest: 'b'.repeat(64),
  installed_at: 1700000000,
  status: 'ready',
  error: null,
  builtin_version: null,
  capabilities: ['infer'],
  affected_bindings: [],
  pending_update: null,
}
function view(update = false): MarketplaceView {
  return {
    sources: [source],
    entries: [
      {
        source_id: source.id,
        vendor_id: release.vendor_id,
        releases: [release],
        incomplete: false,
        conflict: false,
        latest: release,
        update: update ? release : null,
      },
    ],
    update_count: update ? 1 : 0,
  }
}
async function mockMarketplace(page: Page, plugins: PluginSummary[] = [], update = false): Promise<void> {
  await prepareApp(page)
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: plugins } }))
  await page.route('**/api/v1/plugin-marketplace', (route) => route.fulfill({ json: { data: view(update) } }))
}

test('marketplace browsing, search and installed filters use the cached catalog', async ({ page }) => {
  await mockMarketplace(page)
  await page.goto('/vendor-plugins')
  await expect(page.getByRole('heading', { name: 'Vendor plugins' })).toBeVisible()
  await expect(page.getByText('WorkBuddy').first()).toBeVisible()
  await expect(page.getByText('example/plugins').first()).toBeVisible()
  const search = page.getByRole('searchbox', { name: /search plugins/i }).or(page.getByPlaceholder(/search plugins/i))
  await search.fill('not-a-plugin')
  await expect(page.getByText('WorkBuddy')).toHaveCount(0)
  await search.fill('workbuddy')
  await expect(page.getByText('WorkBuddy').first()).toBeVisible()
  await search.clear()
  await page.getByRole('combobox', { name: /source/i }).click()
  await page.getByRole('option', { name: /example\/plugins/i }).click()
  await expect(page.getByText('WorkBuddy').first()).toBeVisible()
  await page.getByRole('tab', { name: /installed/i }).click()
  await expect(page.getByText('WorkBuddy')).toHaveCount(0)
})

test('install prepares the selected release and requires confirmation', async ({ page }) => {
  await mockMarketplace(page)
  let prepared = false
  let confirmed = false
  await page.route('**/api/v1/plugin-marketplace/preview-install', (route) => {
    expect(route.request().postDataJSON()).toMatchObject({
      source_id: source.id,
      release_id: release.release_id,
      vendor_id: release.vendor_id,
      version: release.version,
      catalog_sha256: release.catalog_sha256,
    })
    prepared = true
    return route.fulfill({
      contentType: 'text/event-stream',
      body: `event: preview\ndata: ${JSON.stringify(preview)}\n\n`,
    })
  })
  await page.route('**/api/v1/vendor-plugins/confirm', (route) => {
    expect(route.request().postDataJSON().preview_id).toBe(preview.id)
    confirmed = true
    return route.fulfill({ json: { data: { ...installed, version: '2.0.0' } } })
  })
  await page.goto('/vendor-plugins')
  await page.getByRole('button', { name: 'Install', exact: true }).first().click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toContainText('WorkBuddy')
  expect(prepared).toBe(true)
  expect(confirmed).toBe(false)
  await dialog
    .getByRole('button', { name: /apply|install/i })
    .last()
    .click()
  await expect.poll(() => confirmed).toBe(true)
})

test('an update opens a review and is never applied before confirmation', async ({ page }) => {
  await mockMarketplace(page, [installed], true)
  let confirmed = false
  await page.route('**/api/v1/plugin-marketplace/preview-install', async (route) => {
    expect(route.request().postDataJSON()).toMatchObject({
      source_id: 'community',
      vendor_id: 'workbuddy',
      version: '2.0.0',
    })
    await route.fulfill({
      contentType: 'text/event-stream',
      body: `event: preview\ndata: ${JSON.stringify({ ...preview, previous_version: '1.0.0', previous_origin: origin })}\n\n`,
    })
  })
  await page.route('**/api/v1/vendor-plugins/confirm', async (route) => {
    expect(route.request().postDataJSON().preview_id).toBe(preview.id)
    confirmed = true
    await route.fulfill({ json: { data: { ...installed, version: '2.0.0' } } })
  })
  await page.goto('/vendor-plugins')
  await page.getByRole('tab', { name: /updates/i }).click()
  await page.getByRole('button', { name: 'Update', exact: true }).first().click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toBeVisible()
  await expect(dialog).toContainText('WorkBuddy')
  expect(confirmed).toBe(false)
  await dialog.getByRole('button', { name: /cancel/i }).click()
  expect(confirmed).toBe(false)
  await page.getByRole('button', { name: 'Update', exact: true }).first().click()
  await page
    .getByRole('dialog')
    .getByRole('button', { name: /apply|install|update/i })
    .last()
    .click()
  await expect.poll(() => confirmed).toBe(true)
})

test('adding a source requires reviewing its fingerprint before trust', async ({ page }) => {
  await mockMarketplace(page)
  let added = false
  await page.route('**/api/v1/plugin-sources', (route) => {
    if (route.request().method() === 'POST') {
      expect(route.request().postDataJSON()).toEqual({ preview_id: 'source-preview' })
      added = true
      return route.fulfill({ json: { data: source } })
    }
    return route.fulfill({ json: { data: [] } })
  })
  await page.route('**/api/v1/plugin-sources/preview', (route) => {
    expect(route.request().postDataJSON()).toMatchObject({ url: source.url })
    return route.fulfill({
      json: {
        data: {
          id: 'source-preview',
          preview_id: 'source-preview',
          repository_id: source.repository_id,
          repository: source.repository,
          url: source.url,
          public_key: source.public_key,
          fingerprint: source.fingerprint,
          mirrored: false,
          releases: [release],
        },
      },
    })
  })
  await page.goto('/vendor-plugins')
  await page.getByRole('button', { name: 'Plugin sources' }).click()
  await page.getByRole('button', { name: 'Add source' }).click()
  await page.getByLabel('GitHub repository URL').fill(source.url)
  await page.getByRole('button', { name: 'Preview source' }).click()
  await expect(page.getByText(source.fingerprint).first()).toBeVisible()
  await expect(page.getByText('WorkBuddy').last()).toBeVisible()
  expect(added).toBe(false)
  await page.getByRole('button', { name: 'Trust and add' }).click()
  await expect.poll(() => added).toBe(true)
})

test('marketplace controls are available in Chinese', async ({ page }) => {
  await mockMarketplace(page)
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
  await page.goto('/vendor-plugins')
  await expect(page.getByRole('heading', { name: '供应商插件' })).toBeVisible()
  await expect(page.getByRole('tab', { name: '已安装' })).toBeVisible()
  const search = page.getByRole('searchbox', { name: /搜索插件/ }).or(page.getByPlaceholder(/搜索插件/))
  await search.fill('workbuddy')
  await expect(page.getByText('WorkBuddy').first()).toBeVisible()
})

test('marketplace remains usable at 320px with keyboard navigation', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 700 })
  await mockMarketplace(page)
  await page.goto('/vendor-plugins')
  await expect(page.getByRole('button', { name: 'Install', exact: true })).toBeVisible()
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
  const search = page.getByRole('searchbox', { name: /search plugins/i }).or(page.getByPlaceholder(/search plugins/i))
  await search.focus()
  await page.keyboard.type('workbuddy')
  const details = page.getByRole('button', { name: 'Details', exact: true })
  await details.focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('dialog').getByRole('heading', { name: 'WorkBuddy', exact: true })).toBeVisible()
  await page.getByRole('dialog').getByRole('button', { name: 'Close', exact: true }).focus()
  await page.keyboard.press('Enter')
  await expect(page.getByRole('dialog')).not.toBeVisible()
})
