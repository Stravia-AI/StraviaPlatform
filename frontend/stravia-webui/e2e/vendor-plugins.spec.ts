import { openSync, truncateSync } from 'node:fs'

import { expect, test } from '@playwright/test'

import type { PluginPreview, PluginSummary } from '../src/lib/types'
import { prepareApp } from './prepare-app'

test('an unavailable bundled plugin can be restored without importing a replacement', async ({ page }) => {
  await prepareApp(page)
  let plugin: PluginSummary = {
    vendor_id: 'base',
    name: 'Stravia Base Providers',
    version: '0.3.0',
    source: 'builtin',
    status: 'unavailable',
    error: 'The installed component is unavailable.',
    builtin_version: '0.3.0',
    capabilities: ['infer'],
    affected_bindings: [],
    pending_update: null,
  }
  let preview: PluginPreview = {
    id: 'restore-bundled-component',
    vendor_id: 'base',
    name: plugin.name,
    author: 'Stravia',
    previous_version: plugin.version,
    new_version: '0.3.0',
    target_source: 'builtin',
    is_downgrade: false,
    inherits_credentials: false,
    affected_providers: [],
    network_permissions: [],
    removed_network_permissions: [],
    discarded_data: [],
    affected_bindings: [],
    cancels_active_operations: false,
    active_operations: 0,
    affected_auth_sessions: 2,
  }
  let applied = false
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [plugin] } }))
  await page.route('**/api/v1/vendor-plugins/base/restore', (route) => route.fulfill({ json: { data: preview } }))
  await page.route('**/api/v1/vendor-plugins/confirm', async (route) => {
    expect(route.request().postDataJSON().preview_id).toBe(preview.id)
    applied = true
    plugin = { ...plugin, status: 'ready', error: null }
    await route.fulfill({ json: { data: plugin } })
  })

  await page.goto('/vendor-plugins')
  const restore = page.getByRole('button', { name: 'Restore bundled version' })
  await restore.click()
  const previewDialog = page.getByRole('dialog', { name: 'Review plugin change' })
  await expect(previewDialog).toBeVisible()
  const risks = previewDialog.locator('section[aria-labelledby="preview-risks-title"]')
  await expect(risks).toHaveCount(0)

  expect(applied).toBe(false)
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(restore).toBeVisible()
  expect(applied).toBe(false)

  preview = { ...preview, cancels_active_operations: true, active_operations: 1 }
  await restore.click()
  await expect(previewDialog).toBeVisible()
  await expect(risks.getByRole('listitem')).toHaveCount(2)
  await page.getByRole('button', { name: 'Apply plugin change' }).click()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(page.getByRole('row').filter({ hasText: plugin.name }).getByText('Ready', { exact: true })).toBeVisible()
  await expect(restore).toHaveCount(0)
  expect(applied).toBe(true)
})

test('an unavailable dedicated bundled plugin can be cancelled, retried, and uninstalled while base remains protected', async ({
  page,
}) => {
  await page.setViewportSize({ width: 320, height: 700 })
  await prepareApp(page)
  const basePlugin: PluginSummary = {
    vendor_id: 'base',
    name: 'Stravia Base Providers',
    version: '0.3.0',
    source: 'builtin',
    status: 'ready',
    error: null,
    builtin_version: '0.3.0',
    capabilities: ['infer'],
    affected_bindings: [],
    pending_update: null,
  }
  const dedicatedPlugin: PluginSummary = {
    vendor_id: 'legacy-dedicated',
    name: 'Legacy Dedicated Vendor',
    version: '1.4.0',
    source: 'builtin',
    status: 'unavailable',
    error: 'The installed component is unavailable.',
    builtin_version: '1.4.0',
    capabilities: ['web_search'],
    affected_bindings: [],
    pending_update: null,
  }
  let plugins = [basePlugin, dedicatedPlugin]
  let listRequests = 0
  let uninstallRequests = 0
  let releaseFirstAttempt!: () => void
  const firstAttemptGate = new Promise<void>((resolve) => {
    releaseFirstAttempt = resolve
  })

  await page.route('**/api/v1/vendor-plugins', async (route) => {
    listRequests += 1
    await route.fulfill({ json: { data: plugins } })
  })
  await page.route('**/api/v1/vendor-plugins/legacy-dedicated', async (route) => {
    expect(route.request().method()).toBe('DELETE')
    uninstallRequests += 1
    if (uninstallRequests === 1) {
      await firstAttemptGate
      await route.fulfill({ status: 503, json: { error: 'Temporary uninstall failure' } })
      return
    }
    plugins = plugins.filter((plugin) => plugin.vendor_id !== dedicatedPlugin.vendor_id)
    await route.fulfill({ json: { data: null } })
  })

  await page.goto('/vendor-plugins')
  const baseRow = page.getByRole('listitem').filter({ hasText: basePlugin.name })
  const dedicatedRow = page.getByRole('listitem').filter({ hasText: dedicatedPlugin.name })
  await expect(dedicatedRow.getByText('Bundled', { exact: true })).toBeVisible()
  await expect(dedicatedRow.getByText('Unavailable', { exact: true })).toBeVisible()
  await expect(dedicatedRow.getByRole('button', { name: 'Uninstall', exact: true })).toBeVisible()
  await expect(baseRow.getByRole('button', { name: 'Uninstall', exact: true })).toHaveCount(0)

  await dedicatedRow.getByRole('button', { name: 'Uninstall', exact: true }).click()
  let dialog = page.getByRole('alertdialog', { name: `Uninstall ${dedicatedPlugin.name}?` })
  await expect(dialog).toBeVisible()

  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  expect(uninstallRequests).toBe(0)

  await dedicatedRow.getByRole('button', { name: 'Uninstall', exact: true }).click()
  dialog = page.getByRole('alertdialog', { name: `Uninstall ${dedicatedPlugin.name}?` })
  const confirm = dialog.getByRole('button', { name: /Uninstall/ })
  const cancel = dialog.getByRole('button', { name: 'Cancel', exact: true })
  await confirm.click()
  await expect(confirm).toBeDisabled()
  await expect(cancel).toBeDisabled()
  await expect(dialog).toBeVisible()
  expect(uninstallRequests).toBe(1)

  releaseFirstAttempt()
  await expect(dialog.getByText('Temporary uninstall failure')).toBeVisible()
  await expect(confirm).toBeEnabled()
  await expect(cancel).toBeEnabled()
  await confirm.click()

  await expect(dialog).toHaveCount(0)
  await expect(dedicatedRow).toHaveCount(0)
  await expect(baseRow).toBeVisible()
  await expect(page.getByText('Vendor plugin uninstalled.', { exact: true })).toBeVisible()
  await expect.poll(() => uninstallRequests).toBe(2)
  await expect.poll(() => listRequests).toBeGreaterThan(1)
})

const readyBase: PluginSummary = {
  vendor_id: 'base',
  name: 'Stravia Base Providers',
  version: '0.3.0',
  source: 'builtin',
  status: 'ready',
  error: null,
  builtin_version: '0.3.0',
  capabilities: ['infer', 'model_discovery'],
  affected_bindings: [],
  pending_update: null,
}

function previewOf(overrides: Partial<PluginPreview> = {}): PluginPreview {
  return {
    id: 'preview-1',
    vendor_id: 'fixture.import',
    name: 'Imported Fixture',
    author: 'Fixture Author',
    previous_version: null,
    new_version: '1.0.0',
    target_source: 'local',
    is_downgrade: false,
    inherits_credentials: true,
    affected_providers: [],
    network_permissions: [],
    removed_network_permissions: [],
    discarded_data: [],
    affected_bindings: [],
    cancels_active_operations: false,
    active_operations: 0,
    affected_auth_sessions: 0,
    ...overrides,
  }
}

async function noPageOverflow(page: import('@playwright/test').Page): Promise<boolean> {
  return page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)
}

const labels = {
  'en-US': {
    restore: 'Restore bundled version',
    uninstall: 'Uninstall',
    details: 'Details',
    review: 'Review update',
    bundled: 'Bundled',
    local: 'Local',
    bundledVersion: 'Bundled version',
  },
  'zh-CN': {
    restore: '恢复程序随附版本',
    uninstall: '卸载',
    details: '详情',
    review: '检查更新',
    bundled: '程序随附',
    local: '本地安装',
    bundledVersion: '随附版本',
  },
} as const

for (const locale of ['en-US', 'zh-CN'] as const) {
  for (const viewport of [
    { width: 2560, height: 1440 },
    { width: 390, height: 844 },
    { width: 320, height: 700 },
  ]) {
    test(`plugin versions, sources, errors, recovery and uninstall stay reachable in ${locale} at ${viewport.width}px`, async ({
      page,
    }) => {
      const text = labels[locale]
      const longError = 'The installed component is unavailable. '.repeat(6).trim()
      await page.setViewportSize(viewport)
      await prepareApp(page)
      await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
      const plugins: PluginSummary[] = [
        readyBase,
        {
          vendor_id: 'fixture.local',
          name: 'Local Fixture Vendor',
          version: '2.1.0',
          source: 'local',
          status: 'ready',
          error: null,
          builtin_version: '1.0.0',
          capabilities: ['infer', 'search', 'media_image'],
          affected_bindings: [
            { route_id: 'route-1', provider_id: 'provider-1', upstream_model: 'model-1', capability: 'search' },
          ],
          pending_update: null,
        },
        {
          vendor_id: 'legacy-dedicated',
          name: 'Legacy Dedicated Vendor',
          version: '1.4.0',
          source: 'builtin',
          status: 'unavailable',
          error: longError,
          builtin_version: '1.4.0',
          capabilities: ['search'],
          affected_bindings: [],
          pending_update: null,
        },
        {
          vendor_id: 'fixture.pending',
          name: 'Pending Fixture Vendor',
          version: '1.0.0',
          source: 'builtin',
          status: 'ready',
          error: null,
          builtin_version: '1.0.0',
          capabilities: ['infer'],
          affected_bindings: [],
          pending_update: previewOf({ id: 'pending', vendor_id: 'fixture.pending', new_version: '1.5.0' }),
        },
      ]
      await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: plugins } }))
      await page.goto('/vendor-plugins')

      const desktop = viewport.width > 1000
      const rows = desktop ? page.getByRole('row') : page.getByRole('listitem')
      const local = rows.filter({ hasText: 'Local Fixture Vendor' })
      const dedicated = rows.filter({ hasText: 'Legacy Dedicated Vendor' })
      await expect(local).toBeVisible()
      await expect(local.getByText('2.1.0', { exact: true })).toBeVisible()
      await expect(local.getByText(text.local, { exact: true })).toBeVisible()
      await expect(local.getByText(`${text.bundledVersion} 1.0.0`, { exact: true })).toBeVisible()
      await expect(dedicated.getByText(text.bundled, { exact: true })).toBeVisible()
      await expect(dedicated.getByText(longError)).toBeVisible()
      // 内部 vendor_id 不再常驻在列表里。
      await expect(page.getByText('legacy-dedicated', { exact: true })).toHaveCount(0)

      const pending = rows.filter({ hasText: 'Pending Fixture Vendor' })
      const reachable = [
        [pending, text.review],
        [dedicated, text.restore],
        [dedicated, text.uninstall],
        [dedicated, text.details],
      ] as const
      for (const [row, name] of reachable) {
        const button = row.getByRole('button', { name, exact: true })
        await button.scrollIntoViewIfNeeded()
        await expect(button).toBeVisible()
        const box = (await button.boundingBox())!
        expect(box.x).toBeGreaterThanOrEqual(0)
        expect(box.x + box.width).toBeLessThanOrEqual(viewport.width)
      }
      expect(await noPageOverflow(page)).toBe(true)

      if (desktop) {
        // 资源列表直接落在背景上：没有卡片外框，名称列吸收新增宽度，状态与操作保持紧凑。
        const surface = await page.locator('[data-slot="data-table-viewport"]').evaluate((element) => {
          const style = getComputedStyle(element)
          return { background: style.backgroundColor, shadow: style.boxShadow, radius: parseFloat(style.borderRadius) }
        })
        expect(surface.background).toBe('rgba(0, 0, 0, 0)')
        expect(surface.shadow).toBe('none')
        expect(surface.radius).toBeLessThanOrEqual(8)
        const table = (await page.getByRole('table').boundingBox())!
        const header = page.getByRole('columnheader')
        const nameBox = (await header.nth(0).boundingBox())!
        const statusBox = (await header.nth(4).boundingBox())!
        const actionBox = (await header.nth(5).boundingBox())!
        expect(table.width).toBeGreaterThan(1500)
        expect(nameBox.width).toBeGreaterThan(statusBox.width * 3)
        expect(actionBox.width).toBeLessThan(table.width / 3)
      }
    })
  }
}

test('importing several files validates each one, advances the queue, and keeps cancelled previews unapplied', async ({
  page,
}) => {
  await prepareApp(page)
  let importRequests = 0
  const confirmed: string[] = []
  const previews = [
    previewOf({ id: 'second', vendor_id: 'fixture.second', name: 'Second Fixture' }),
    previewOf({ id: 'third', vendor_id: 'fixture.third', name: 'Third Fixture' }),
  ]
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [readyBase] } }))
  await page.route('**/api/v1/vendor-plugins/import', async (route) => {
    importRequests += 1
    if (importRequests === 1) {
      await route.fulfill({ status: 422, json: { error: 'Component validation failed' } })
      return
    }
    await route.fulfill({ json: { data: previews[importRequests - 2] } })
  })
  await page.route('**/api/v1/vendor-plugins/confirm', async (route) => {
    confirmed.push(route.request().postDataJSON().preview_id)
    await route.fulfill({ json: { data: readyBase } })
  })

  await page.goto('/vendor-plugins')
  await page.locator('#vendor-plugin-file').setInputFiles([
    { name: 'broken.wasm', mimeType: 'application/wasm', buffer: Buffer.from('broken') },
    { name: 'second.wasm', mimeType: 'application/wasm', buffer: Buffer.from('second') },
    { name: 'third.wasm', mimeType: 'application/wasm', buffer: Buffer.from('third') },
  ])

  const dialog = page.getByRole('dialog', { name: 'Review plugin change' })
  await expect(dialog.getByRole('heading', { name: 'Second Fixture' })).toBeVisible()
  await expect(page.getByText(/broken\.wasm: .*Component validation failed/)).toBeVisible()
  expect(confirmed).toEqual([])

  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(dialog.getByRole('heading', { name: 'Third Fixture' })).toBeVisible()
  expect(confirmed).toEqual([])

  await dialog.getByRole('button', { name: 'Apply plugin change' }).click()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(confirmed).toEqual(['third'])
  expect(importRequests).toBe(3)
})

test('a file over the 64 MiB limit is rejected before upload', async ({ page }, testInfo) => {
  const hugePath = testInfo.outputPath('huge.wasm')
  truncateSync(openSync(hugePath, 'w'), 64 * 1024 * 1024 + 1)
  await prepareApp(page)
  let importRequests = 0
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [readyBase] } }))
  await page.route('**/api/v1/vendor-plugins/import', async (route) => {
    importRequests += 1
    await route.fulfill({ json: { data: previewOf() } })
  })
  await page.goto('/vendor-plugins')
  await page.locator('#vendor-plugin-file').setInputFiles(hugePath)
  await expect(page.getByText(/huge\.wasm: This file exceeds the 64 MiB upload limit/)).toBeVisible()
  expect(importRequests).toBe(0)
  await expect(page.getByRole('dialog')).toHaveCount(0)
})

test('the review shows network, data, credential, session and charge impact and blocks discarding data without consent', async ({
  page,
}) => {
  await prepareApp(page)
  let confirmRequests = 0
  let lastConfirm: { preview_id: string; allow_data_discard: boolean } | undefined
  const risky = previewOf({
    id: 'risky',
    vendor_id: 'fixture.risky',
    name: 'Risky Fixture',
    previous_version: '2.0.0',
    new_version: '1.0.0',
    is_downgrade: true,
    inherits_credentials: false,
    affected_providers: [{ id: 'provider-1', name: 'Fixture Connection' }],
    network_permissions: [
      { origin: 'https://new.example', provider_id: 'provider-1', configuration_field: 'base_url', added: true },
      { origin: 'https://kept.example', provider_id: null, configuration_field: null, added: false },
    ],
    removed_network_permissions: ['https://old.example'],
    discarded_data: [
      {
        provider: { id: 'provider-1', name: 'Fixture Connection' },
        kinds: ['private_state'],
        recovery_actions: ['Reconnect'],
      },
    ],
    affected_bindings: [
      { route_id: 'route-1', provider_id: 'provider-1', upstream_model: 'model-1', capability: 'search' },
    ],
    cancels_active_operations: true,
    active_operations: 3,
    affected_auth_sessions: 2,
  })
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [readyBase] } }))
  await page.route('**/api/v1/vendor-plugins/import', (route) => route.fulfill({ json: { data: risky } }))
  await page.route('**/api/v1/vendor-plugins/confirm', async (route) => {
    confirmRequests += 1
    lastConfirm = route.request().postDataJSON()
    if (confirmRequests === 1) {
      await route.fulfill({ status: 503, json: { error: 'Temporary apply failure' } })
      return
    }
    await route.fulfill({ json: { data: readyBase } })
  })

  await page.goto('/vendor-plugins')
  await page
    .locator('#vendor-plugin-file')
    .setInputFiles({ name: 'risky.wasm', mimeType: 'application/wasm', buffer: Buffer.from('risky') })
  const dialog = page.getByRole('dialog', { name: 'Review plugin change' })
  await expect(dialog).toBeVisible()
  await expect(dialog.getByText('Downgrade', { exact: true })).toBeVisible()
  await expect(dialog.getByText('Version 2.0.0 → 1.0.0')).toBeVisible()
  await expect(dialog.getByText('New network origins')).toBeVisible()
  await expect(dialog.getByText('https://new.example')).toBeVisible()
  await expect(dialog.getByText('Removed network origins')).toBeVisible()
  await expect(dialog.getByText('https://old.example')).toBeVisible()
  await expect(dialog.getByText('Retained network origins')).toBeVisible()
  await expect(dialog.getByText('https://kept.example')).toBeVisible()
  await expect(dialog.getByText('Fixture Connection · provider-1')).toBeVisible()
  await expect(dialog.getByText('Existing credential trust is not inherited.', { exact: false })).toBeVisible()
  await expect(dialog.getByText('3 active operations', { exact: false })).toBeVisible()
  await expect(dialog.getByText('Cancellation does not guarantee', { exact: false })).toBeVisible()
  await expect(dialog.getByText('2 authorization sessions', { exact: false })).toBeVisible()
  await expect(dialog.getByText('1 configured bindings lose a capability')).toBeVisible()

  const apply = dialog.getByRole('button', { name: 'Apply plugin change' })
  await expect(apply).toBeDisabled()
  await apply.click({ force: true })
  expect(confirmRequests).toBe(0)

  await dialog.getByRole('checkbox', { name: 'I understand and allow the listed plugin data to be discarded.' }).check()
  await expect(apply).toBeEnabled()
  await apply.click()
  // 应用失败：审查与勾选保留，可重试。
  await expect(dialog.getByText('Temporary apply failure')).toBeVisible()
  await expect(dialog).toBeVisible()
  await expect(dialog.getByRole('checkbox')).toBeChecked()
  expect(lastConfirm).toEqual({ preview_id: 'risky', allow_data_discard: true })
  await apply.click()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(confirmRequests).toBe(2)
})

test('attention is a compact count that locates the row owning the reason and recovery', async ({ page }) => {
  await prepareApp(page)
  const failing: PluginSummary = {
    ...readyBase,
    vendor_id: 'legacy-dedicated',
    name: 'Legacy Dedicated Vendor',
    status: 'error',
    error: 'Component failed to load',
  }
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [readyBase, failing] } }))
  await page.goto('/vendor-plugins')
  const attention = page.locator('[data-slot="plugin-attention"]')
  await expect(attention).toContainText('1 plugins failed to load')
  // 原因只在所属行出现，不复制整段告警。
  await expect(page.getByText('Component failed to load').locator('visible=true')).toHaveCount(1)
  await attention.getByRole('button', { name: 'Locate plugin' }).click()
  await expect(page.getByRole('row').filter({ hasText: 'Legacy Dedicated Vendor' })).toBeInViewport()
})

test('refresh failure keeps the previous list and details open read-only without save prompts', async ({ page }) => {
  await prepareApp(page)
  let failRefresh = false
  await page.route('**/api/v1/vendor-plugins', (route) =>
    failRefresh
      ? route.fulfill({ status: 503, json: { error: 'Refresh unavailable' } })
      : route.fulfill({ json: { data: [readyBase] } }),
  )
  await page.goto('/vendor-plugins')
  await expect(page.getByRole('row').filter({ hasText: readyBase.name })).toBeVisible()

  await page.getByRole('button', { name: 'Details', exact: true }).click()
  const sheet = page.getByRole('dialog')
  await expect(sheet.getByText('Plugin ID')).toBeVisible()
  await expect(sheet.getByText('base', { exact: true })).toBeVisible()
  await sheet.getByRole('button', { name: 'Close' }).click()
  await expect(sheet).toHaveCount(0)
  await expect(page.getByText('Discard unsaved changes?')).toHaveCount(0)

  failRefresh = true
  await page.getByRole('button', { name: 'Refresh plugins' }).click()
  await expect(page.getByText('Vendor plugins could not be loaded.')).toBeVisible()
  await expect(page.getByRole('row').filter({ hasText: readyBase.name })).toBeVisible()
})

test('an empty plugin list keeps the implementation note behind the help mark and offers import', async ({ page }) => {
  await prepareApp(page)
  await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [] } }))
  await page.goto('/vendor-plugins')
  await expect(page.getByText('No vendor plugins are available')).toBeVisible()
  await expect(page.getByText('Import a local WebAssembly component', { exact: false })).toHaveCount(0)
  await page.getByRole('button', { name: 'More about this field' }).focus()
  await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Import a local WebAssembly component')
  await expect(page.getByRole('button', { name: 'Import a plugin' })).toHaveCount(1)
})
