import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('custom currency requires first-save confirmation and shows the frozen summary', async ({ page }) => {
  let currency: Record<string, string> | null = null
  let savedInput: unknown

  await page.route('**/api/v1/currency', async (route) => {
    if (route.request().method() === 'PUT') {
      savedInput = route.request().postDataJSON()
      currency = { symbol: '¥', usd_rate: '8', first_rate: '8', configured_at: '2026-10-28T08:00:00Z' }
    }
    await route.fulfill({ json: { data: { custom: currency } } })
  })

  await page.setViewportSize({ width: 1279, height: 900 })
  await page.goto('/settings?view=general')
  const row = page.locator('[data-slot="setting-row"]').filter({ hasText: 'Display currency' })
  await expect(row).toContainText('Not configured')
  await row.getByRole('button', { name: 'Edit Display currency', exact: true }).click()

  const sheet = page.locator('[data-slot="sheet-content"]')
  await expect(sheet.getByRole('heading', { name: 'Set custom currency' })).toBeVisible()
  await sheet.getByLabel('Currency symbol').fill('¥')
  await sheet.getByLabel('1 USD equals').fill('8')
  await sheet.getByRole('button', { name: 'Save currency' }).click()

  const confirmation = page.getByRole('alertdialog', { name: 'Set custom currency?' })
  await expect(confirmation).toContainText('it cannot be removed')
  await expect(confirmation).toContainText('spend limits are converted')
  await confirmation.getByRole('button', { name: 'Set custom currency' }).click()

  await expect(row).toContainText('¥ · 1 USD = 8.00')
  expect(savedInput).toEqual({ symbol: '¥', usd_rate: '8' })
})

test('API Key spend limit, spent amount, and reset use the display currency', async ({ page }) => {
  let apiKey = {
    id: 'key-billing',
    key: 'sk-billing',
    name: 'Billing client',
    rpm_limit: null,
    spend_limit: '36' as string | null,
    spend: { spent: '12.5', reset_at: null as string | null },
    is_enabled: true,
    mcp_access_enabled: false,
    transparent_injection_enabled: false,
    inject_web_search: false,
    inject_media_understanding: false,
    inject_media_generation: false,
    expires_at: null,
    created_at: '2026-10-01T00:00:00Z',
    updated_at: '2026-10-01T00:00:00Z',
    model_ids: [],
  }
  let savedInput: Record<string, unknown> | undefined

  await page.route('**/api/v1/currency', async (route) => {
    await route.fulfill({
      json: {
        data: { custom: { symbol: '¥', usd_rate: '8', first_rate: '8', configured_at: '2026-10-28T08:00:00Z' } },
      },
    })
  })
  await page.route('**/api/v1/api-keys/key-billing/spend/reset', async (route) => {
    apiKey = { ...apiKey, spend: { spent: '0', reset_at: '2026-10-28T09:00:00Z' } }
    await route.fulfill({ json: { data: apiKey.spend } })
  })
  await page.route('**/api/v1/api-keys/key-billing', async (route) => {
    if (route.request().method() === 'PUT') {
      savedInput = route.request().postDataJSON() as Record<string, unknown>
      apiKey = { ...apiKey, spend_limit: savedInput.spend_limit as string | null }
    }
    await route.fulfill({ json: { data: apiKey } })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({ json: { data: [apiKey] } })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })
  await page.route('**/api/v1/web-search/config', async (route) => {
    await route.fulfill({ json: { data: { enabled: false } } })
  })
  await page.route('**/api/v1/media-understanding', async (route) => {
    await route.fulfill({ json: { data: { image: { enabled: false }, audio: { enabled: false } } } })
  })
  await page.route('**/api/v1/media-generation/config', async (route) => {
    await route.fulfill({
      json: {
        data: {
          config: { enabled: false, image: { route_id: null } },
          validation: { valid: false, code: 'media_generation_route_missing', message: null },
        },
      },
    })
  })

  await page.setViewportSize({ width: 1279, height: 900 })
  await page.goto('/api-keys/key-billing')
  await expect(page.getByRole('heading', { name: 'Billing client' })).toBeVisible()
  const limit = page.locator('#api-key-spend-limit')
  await expect(limit).toHaveValue('36')
  await expect(page.getByText('¥12.50', { exact: true })).toBeVisible()
  await expect(page.getByText('Never', { exact: true })).toBeVisible()

  await page.getByRole('button', { name: 'Reset spent', exact: true }).click()
  const confirmation = page.getByRole('alertdialog', { name: 'Reset spent amount?' })
  await expect(confirmation).toContainText('Billing records and usage statistics are kept')
  await confirmation.getByRole('button', { name: 'Reset spent', exact: true }).click()
  await expect(page.getByText('¥0.00', { exact: true })).toBeVisible()
  await expect(page.getByText('Never', { exact: true })).toHaveCount(0)

  await limit.fill('50')
  await page.getByRole('button', { name: 'Save API Key', exact: true }).click()
  await expect(page).toHaveURL(/\/api-keys$/)
  expect(savedInput?.spend_limit).toBe('50')
})

test('API Key list shows the available balance in the display currency and Unlimited without a spend limit', async ({
  page,
}) => {
  const base = {
    rpm_limit: null,
    is_enabled: true,
    mcp_access_enabled: false,
    transparent_injection_enabled: false,
    inject_web_search: false,
    inject_media_understanding: false,
    inject_media_generation: false,
    expires_at: null,
    created_at: '2026-10-01T00:00:00Z',
    updated_at: '2026-10-01T00:00:00Z',
    model_ids: [],
  }
  const apiKeys = [
    {
      ...base,
      id: 'key-capped',
      key: 'sk-capped',
      name: 'Capped client',
      spend_limit: '36',
      spend: { spent: '12.499', reset_at: null },
      spend_available: '23.501',
    },
    {
      ...base,
      id: 'key-exhausted',
      key: 'sk-exhausted',
      name: 'Exhausted client',
      spend_limit: '10',
      spend: { spent: '12', reset_at: null },
      spend_available: '0',
    },
    {
      ...base,
      id: 'key-unlimited',
      key: 'sk-unlimited',
      name: 'Unlimited client',
      spend_limit: null,
      spend: { spent: '3', reset_at: null },
      spend_available: null,
    },
  ]

  await page.route('**/api/v1/currency', async (route) => {
    await route.fulfill({
      json: {
        data: { custom: { symbol: '¥', usd_rate: '8', first_rate: '8', configured_at: '2026-10-28T08:00:00Z' } },
      },
    })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({ json: { data: apiKeys } })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })

  await page.setViewportSize({ width: 1279, height: 900 })
  await page.goto('/api-keys')
  await expect(page.getByRole('columnheader', { name: 'Available balance' })).toBeVisible()
  await expect(page.getByRole('columnheader', { name: 'Model access' })).toHaveCount(0)
  const row = (name: string) => page.getByRole('row').filter({ hasText: name })
  await expect(row('Capped client')).toContainText('¥23.50')
  await expect(row('Exhausted client')).toContainText('¥0.00')
  await expect(row('Unlimited client')).toContainText('Unlimited')
})
