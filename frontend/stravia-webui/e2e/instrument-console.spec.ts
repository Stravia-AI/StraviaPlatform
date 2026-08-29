import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
  await page.setViewportSize({ width: 1280, height: 800 })
})

test('empty Overview hides charts, keeps error rate neutral, and shows the next setup action', async ({ page }) => {
  await page.goto('/')

  await expect(page.getByRole('heading', { name: 'Overview', exact: true })).toBeVisible()
  await expect(page.getByRole('navigation', { name: 'Request path' })).toBeVisible()
  await expect(page.getByText('01', { exact: true })).toBeVisible()
  await expect(page.getByText('Live · 10s')).toHaveCount(0)
  await expect(page.getByLabel('Request volume chart')).toHaveCount(0)
  await expect(page.getByLabel('Latency chart')).toHaveCount(0)

  const errorRate = page.locator('.route-metric-strip__item').filter({ hasText: 'Error rate' })
  await expect(errorRate).toContainText('–')
  await expect(errorRate.locator('.text-destructive')).toHaveCount(0)
  await expect(page.getByRole('navigation', { name: 'Request path' }).getByText('0 available').first()).toBeVisible()
  await expect(page.getByRole('link', { name: 'Create API Key' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Connect a model service' })).toBeVisible()
})

test('Overview with traffic shows charts and a neutral error rate when there are no errors', async ({ page }) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  await page.goto('/')

  await expect(page.getByLabel('Request volume chart')).toBeVisible()
  await expect(page.getByLabel('Latency chart')).toBeVisible()
  const errorRate = page.locator('.route-metric-strip__item').filter({ hasText: 'Error rate' })
  await expect(errorRate).toContainText('0%')
  await expect(errorRate.locator('.text-destructive')).toHaveCount(0)
})

test('empty Model services, Models, API Keys, and logs speak the missing dependency', async ({ page }) => {
  await page.goto('/providers')
  await expect(page.getByText('A Model has nowhere to go until you connect a Provider.')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Connect first service' })).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Connect service' })).toHaveCount(0)

  await page.goto('/models')
  await expect(page.getByText('Connect a model service first')).toBeVisible()
  await expect(page.getByRole('link', { name: 'Go to model services' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Add model' })).toHaveCount(0)

  await page.goto('/api-keys')
  await expect(page.getByText('Create a client credential, then copy its one-time secret immediately.')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Create first API Key' })).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Create API Key' })).toHaveCount(0)

  await page.goto('/logs')
  await expect(page.getByRole('button', { name: 'Clear history' })).toBeDisabled()
  await expect(page.getByLabel('Model service')).toHaveCount(0)
  await expect(
    page.getByRole('region', { name: 'Recent requests' }).getByRole('link', { name: 'Connect agents' }),
  ).toBeVisible()
})

test('Connect an app lists missing Model and API Key instead of empty dropdowns', async ({ page }) => {
  await page.goto('/connect')

  await expect(page.getByRole('link', { name: 'Add a Model' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Create an API Key' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Model' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'API Key' })).toHaveCount(0)
})

test('API Key editor keeps compact controls and help inside the editor', async ({ page }) => {
  await page.goto('/api-keys')
  await page.getByRole('button', { name: 'Create first API Key' }).click()

  const overlay = page.locator('[data-slot="sheet-content"]')
  await expect(overlay.getByRole('heading', { name: 'Create API Key' })).toBeVisible()

  const nameBox = await page.locator('#api-key-name').boundingBox()
  const overlayBox = await overlay.boundingBox()
  expect(nameBox?.width ?? 0).toBeGreaterThan(8)
  expect(nameBox?.width ?? 0).toBeGreaterThan((overlayBox?.width ?? 0) * 0.8)

  const concurrency = page.locator('#api-key-concurrency-limit')
  const expiresAt = page.getByLabel('Expires at')
  await expect(concurrency).toBeVisible()
  await expect(expiresAt).toBeVisible()
  const concurrencyBox = await concurrency.boundingBox()
  const expiresAtBox = await expiresAt.boundingBox()
  expect(concurrencyBox?.width ?? 0).toBeLessThan(nameBox?.width ?? 0)
  expect(Math.abs((concurrencyBox?.width ?? 0) - (expiresAtBox?.width ?? 0))).toBeLessThan(1)
  expect(Math.abs((concurrencyBox?.y ?? 0) - (expiresAtBox?.y ?? 0))).toBeLessThan(1)

  await expect(
    page.getByText('Leave empty for unlimited. Each Proxy request and MCP tools/call uses one slot; nested work reuses it.'),
  ).toBeHidden()
  const help = overlay
    .getByRole('group')
    .filter({ has: concurrency })
    .getByRole('button', { name: 'More about this field' })
  await help.hover()
  await expect(page.locator('[data-slot="tooltip-content"]')).toBeVisible()

  const save = overlay.getByRole('button', { name: 'Save API Key' })
  const cancel = overlay.getByRole('button', { name: 'Cancel' })
  const saveBox = await save.boundingBox()
  const cancelBox = await cancel.boundingBox()
  expect(saveBox?.width ?? 0).toBeLessThan((overlayBox?.width ?? 0) * 0.5)
  expect(cancelBox?.width ?? 0).toBeLessThan((overlayBox?.width ?? 0) * 0.5)
  expect(Math.abs((saveBox?.y ?? 0) - (cancelBox?.y ?? 0))).toBeLessThan(8)
})

test('deleting an API Key quotes the name on a solid destructive confirm', async ({ page }) => {
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'key-gpt',
            key: 'sk-****abcd',
            name: 'GPT key',
            concurrency_limit: null,
            is_enabled: true,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_media_understanding: false,
            inject_web_search: false,
            expires_at: null,
            created_at: '2026-08-17T00:00:00Z',
            updated_at: '2026-08-17T00:00:00Z',
            model_ids: [],
          },
        ],
      },
    })
  })

  await page.goto('/api-keys')
  await page.getByRole('button', { name: 'More actions for GPT key' }).click()
  await page.getByRole('menuitem', { name: 'Delete API Key…' }).click()
  await expect(page.getByRole('heading', { name: 'Delete API Key "GPT key"?' })).toBeVisible()
  const confirm = page.getByRole('button', { name: 'Delete API Key', exact: true })
  const background = await confirm.evaluate((element) => getComputedStyle(element).backgroundColor)
  expect(background).not.toBe('rgba(0, 0, 0, 0)')
  expect(background).not.toMatch(/rgba\([^)]+,\s*0(\.0+)?\)/)
  expect(background).not.toBe('transparent')
})

test('a Provider without a catalog logo uses a steel initial instead of a black square', async ({ page }) => {
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'prov-unknown',
            name: 'Unknown Service',
            protocol: 'openai',
            base_url: 'https://unknown.example/v1',
            is_enabled: true,
            use_proxy: false,
            created_at: '2026-08-17T00:00:00Z',
            updated_at: '2026-08-17T00:00:00Z',
          },
        ],
      },
    })
  })

  await page.route('https://unknown.example/**', async (route) => {
    await route.abort()
  })
  await page.goto('/providers')
  const mark = page.locator('.route-desktop-table .route-provider-mark').first()
  await expect.poll(async () => (await mark.textContent())?.trim()).toBe('U')
  await expect(mark).toHaveAttribute('data-fallback', 'true')
  await expect(mark.locator('img')).toHaveCount(0)
})

async function stubTraffic(page: Page, counts: { requests: number; errors: number }): Promise<void> {
  await page.route('**/api/v1/stats/overview**', async (route) => {
    await route.fulfill({
      json: {
        data: {
          total_requests: counts.requests,
          total_input_tokens: 100,
          total_output_tokens: 40,
          avg_duration_ms: 120,
          error_count: counts.errors,
        },
      },
    })
  })
  await page.route('**/api/v1/stats/hourly**', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            hour: '2026-08-26T00:00:00Z',
            request_count: counts.requests,
            error_count: counts.errors,
            total_input_tokens: 100,
            total_output_tokens: 40,
            avg_duration_ms: 120,
          },
        ],
      },
    })
  })
}
