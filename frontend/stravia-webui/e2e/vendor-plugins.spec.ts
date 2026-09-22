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
  await expect(previewDialog.getByText('No authorization sessions will be cancelled.', { exact: true })).toBeVisible()
  await expect(previewDialog.getByText(/incomplete sign-ins must be started again/i)).toHaveCount(0)
  expect(applied).toBe(false)
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(restore).toBeVisible()
  expect(applied).toBe(false)

  preview = { ...preview, cancels_active_operations: true, active_operations: 1 }
  await restore.click()
  await expect(previewDialog.getByText(/incomplete sign-ins must be started again/i)).toBeVisible()
  await expect(previewDialog.getByText('No authorization sessions will be cancelled.', { exact: true })).toHaveCount(0)
  await page.getByRole('button', { name: 'Apply plugin change' }).click()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(page.getByText('Ready', { exact: true })).toBeVisible()
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
  const baseCard = page.locator('[data-slot="card"]').filter({ hasText: basePlugin.name })
  const dedicatedCard = page.locator('[data-slot="card"]').filter({ hasText: dedicatedPlugin.name })
  await expect(dedicatedCard.getByText('Bundled', { exact: true })).toBeVisible()
  await expect(dedicatedCard.getByText('Unavailable', { exact: true })).toBeVisible()
  await expect(dedicatedCard.getByRole('button', { name: 'Uninstall', exact: true })).toBeVisible()
  await expect(baseCard.getByRole('button', { name: 'Uninstall', exact: true })).toHaveCount(0)

  await dedicatedCard.getByRole('button', { name: 'Uninstall', exact: true }).click()
  let dialog = page.getByRole('alertdialog', { name: `Uninstall ${dedicatedPlugin.name}?` })
  await expect(dialog).toBeVisible()

  await dialog.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(dialog).toHaveCount(0)
  expect(uninstallRequests).toBe(0)

  await dedicatedCard.getByRole('button', { name: 'Uninstall', exact: true }).click()
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
  await expect(dedicatedCard).toHaveCount(0)
  await expect(baseCard).toBeVisible()
  await expect(page.getByText('Vendor plugin uninstalled.', { exact: true })).toBeVisible()
  await expect.poll(() => uninstallRequests).toBe(2)
  await expect.poll(() => listRequests).toBeGreaterThan(1)
})
