import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

interface AllowanceFixture {
  provider_id: string
  [key: string]: unknown
}

const freshSnapshot = {
  provider_id: 'provider-alpha',
  provider_name: 'Alpha account',
  catalog_provider_id: 'anthropic',
  channel: 'claude-code',
  plan_label: 'Max',
  status: 'fresh',
  fetched_at: '2026-09-01T12:00:00Z',
  allowances: [
    {
      key: 'weekly',
      label: 'Weekly',
      kind: 'quota_window',
      used: { value: 55.625, unit: 'tokens' },
      remaining: { value: 44.375, unit: 'tokens' },
      limit: { value: 100, unit: 'tokens' },
      used_percent: 55.625,
    },
  ],
  models: [
    {
      model: 'claude-opus-4-6',
      allowances: [
        {
          key: 'weekly_model',
          label: 'Weekly',
          kind: 'quota_window',
          remaining: { value: 25, unit: 'tokens' },
          reset_at: 1788282000000,
        },
      ],
    },
  ],
}

const staleSnapshot = {
  provider_id: 'provider-beta',
  provider_name: 'Beta account',
  catalog_provider_id: 'openai',
  channel: 'codex',
  status: 'stale',
  fetched_at: '2026-09-01T11:30:00Z',
  allowances: [
    {
      key: 'weekly',
      label: 'Weekly',
      kind: 'quota_window',
      used_percent: 111.25,
      used: { value: 111.25, unit: 'requests' },
      limit: { value: 100, unit: 'requests' },
    },
  ],
  models: [],
  error: { category: 'rate_limited', message: 'safe backend message' },
}

const errorSnapshot = {
  provider_id: 'provider-gamma',
  provider_name: 'Gamma account',
  catalog_provider_id: 'github-copilot',
  channel: 'default',
  status: 'error',
  allowances: [],
  models: [],
  error: { category: 'authentication', message: 'safe backend message' },
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

async function mockAllowances(page: Page, snapshots: AllowanceFixture[]) {
  const posts: string[] = []
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname
    if (request.method() === 'POST') posts.push(path)
    const providerId = path.match(/provider-allowances\/([^/]+)\/refresh$/)?.[1]
    await route.fulfill({
      json: { data: providerId ? snapshots.find((snapshot) => snapshot.provider_id === providerId) : snapshots },
    })
  })
  return posts
}

test('renders fresh, stale, error, typed values, model details, and both refresh actions', async ({ page }) => {
  const posts = await mockAllowances(page, [errorSnapshot, staleSnapshot, freshSnapshot])
  await page.goto('/allowances')

  await expect(page.getByRole('heading', { name: 'Provider allowances' })).toBeVisible()
  const cards = page.locator('[data-testid^="allowance-card-"]')
  await expect(cards).toHaveCount(3)
  await expect(cards.locator('[data-slot="card-title"]')).toHaveText(['Alpha account', 'Beta account', 'Gamma account'])
  await expect(page.getByText('Fresh', { exact: true })).toBeVisible()
  await expect(page.getByText('Stale', { exact: true })).toBeVisible()
  await expect(page.getByText('Unavailable', { exact: true })).toBeVisible()
  await expect(page.getByText('55.63 tokens')).toBeVisible()
  await expect(page.getByText('111.25%').first()).toBeVisible()
  await expect(page.getByText('Showing the last successful result because this refresh failed.')).toBeVisible()
  await expect(page.getByText('Reconnect this model service or update its credential.')).toBeVisible()
  await expect(page.locator('[role="progressbar"]')).toHaveCount(2)
  await expect(page.getByTestId('allowance-card-error').locator('[role="progressbar"]')).toHaveCount(0)

  await page.getByText('claude-opus-4-6').click()
  await expect(page.getByText(/Resets/)).toBeVisible()

  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/refresh')
  await page.getByRole('button', { name: 'Refresh Alpha account' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
})

test('renders the empty and request-error states with recovery guidance', async ({ page }) => {
  await mockAllowances(page, [])
  await page.goto('/allowances')
  await expect(page.getByRole('heading', { name: 'No allowance data available' })).toBeVisible()
  await expect(page.getByText(/Enable and connect a supported model service/)).toBeVisible()
  await expect(page.getByRole('link', { name: 'Manage model services' })).toHaveAttribute('href', '/providers')

  await page.unroute('**/api/v1/provider-allowances**')
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    await route.fulfill({ status: 503, json: { error: 'allowance backend unavailable' } })
  })
  await page.reload()
  await expect(page.getByRole('heading', { name: 'Provider allowances could not be loaded.' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry' })).toBeVisible()
})

test('keeps the card grid usable on a narrow Chinese viewport', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 760 })
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
  await mockAllowances(page, [freshSnapshot])
  await page.goto('/allowances')

  await expect(page.getByRole('heading', { name: 'Provider 额度' })).toBeVisible()
  await expect(page.getByRole('button', { name: '全部刷新' })).toBeVisible()
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  )
  expect(overflow).toBeLessThanOrEqual(0)
})
