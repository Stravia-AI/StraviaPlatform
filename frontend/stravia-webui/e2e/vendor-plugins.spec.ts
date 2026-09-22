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
