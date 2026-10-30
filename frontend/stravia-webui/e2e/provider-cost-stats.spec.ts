import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

const CURRENCY = { kind: 'usd' as const, symbol: '$' }
const END = Date.UTC(2026, 8, 17, 8, 15)
const money = (amount: string, known = 0, unknown = 0) => ({ amount, known_count: known, unknown_count: unknown })

test.use({ timezoneId: 'UTC' })

const providerBase = {
  deleted: false,
  upstream_request_count: 10,
  upstream_error_count: 0,
  avg_duration_ms: 100,
  avg_output_tps: 10,
  currency: CURRENCY,
}

const providers = [
  { ...providerBase, provider_id: 'p-none', provider: 'Untracked service', cost: null, subscription: null },
  {
    ...providerBase,
    provider_id: 'p-usage',
    provider: 'Metered service',
    cost: money('12.345', 4),
    subscription: null,
  },
  {
    ...providerBase,
    provider_id: 'p-worth',
    provider: 'Worth it plan',
    cost: null,
    subscription: { cost: money('10'), usage_equivalent_cost: money('25.004', 3), multiple: '2.5004' },
  },
  {
    ...providerBase,
    provider_id: 'p-partial',
    provider: 'Partly priced plan',
    cost: null,
    subscription: { cost: money('10'), usage_equivalent_cost: money('5', 1, 2), multiple: '0.5' },
  },
  {
    ...providerBase,
    provider_id: 'p-idle',
    provider: 'Idle plan',
    cost: null,
    subscription: { cost: money('0'), usage_equivalent_cost: money('0'), multiple: null },
  },
]

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
  await page.clock.setFixedTime(END)
  await page.route('**/api/v1/stats/**', async (route) => {
    const url = new URL(route.request().url())
    if (url.pathname.endsWith('/overview')) {
      await route.fulfill({
        json: {
          data: {
            total_requests: 50,
            terminal_request_count: 50,
            failed_request_count: 0,
            upstream_request_count: 50,
            upstream_error_count: 0,
            total_input_tokens: 600,
            total_output_tokens: 86,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: null,
            avg_duration_ms: 120,
            avg_first_token_ms: 40,
            avg_output_tps: 25,
            currency: CURRENCY,
            actual_cost: money('22.345', 4),
            subscription: { cost: money('20'), usage_equivalent_cost: money('30.004', 4), multiple: '1.5002' },
          },
        },
      })
    } else if (url.pathname.endsWith('/providers')) {
      await route.fulfill({ json: { data: providers } })
    } else {
      await route.fulfill({ json: { data: [] } })
    }
  })
})

test('provider table rounds costs to cents and highlights the subscription value multiple', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/stats')
  const table = page.getByRole('table', { name: 'Providers' })
  const row = (name: string) => table.getByRole('row').filter({ hasText: name })

  // Not tracked: the row exists but carries no cost of any kind.
  await expect(row('Untracked service')).toBeVisible()
  await expect(row('Untracked service').getByRole('cell').filter({ hasText: /[$×]/ })).toHaveCount(0)

  // Metered: cost only, rounded half away from zero.
  await expect(row('Metered service')).toContainText('$12.35')
  await expect(row('Metered service').getByRole('cell').filter({ hasText: '×' })).toHaveCount(0)

  // Subscription: equivalent usage, subscription cost and multiple; ≥1× is emphasised.
  const worth = row('Worth it plan')
  await expect(worth).toContainText('$25.00')
  await expect(worth).toContainText('$10.00')
  const multiple = worth.getByRole('cell', { name: '2.50×', exact: true })
  await expect(multiple).toBeVisible()
  await expect(multiple).toHaveClass(/text-success/)

  // Unpriced usage makes the multiple a lower bound, and below 1× is not emphasised.
  const partial = row('Partly priced plan').getByRole('cell', { name: '≥0.50×', exact: true })
  await expect(partial).toBeVisible()
  await expect(partial).not.toHaveClass(/text-success/)

  // Zero subscription cost in the window cannot form a ratio.
  await expect(row('Idle plan').getByRole('cell').last()).toHaveText('—')
})

test('overview cost card rounds to cents and reports the subscription value', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/stats')
  const metric = page
    .locator('.route-metric-strip__item')
    .filter({ has: page.getByText('Actual cost', { exact: true }) })
  await expect(metric.locator('p').nth(1)).toHaveText('$22.35')
  await expect(metric).toContainText('Subscription value 1.50×')
})
