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
  {
    ...providerBase,
    provider_id: 'p-none',
    provider: 'Untracked service',
    cost: null,
    subscription: null,
    allocated_cost: null,
  },
  {
    ...providerBase,
    provider_id: 'p-usage',
    provider: 'Metered service',
    cost: money('12.345', 4),
    subscription: null,
    allocated_cost: money('12.345', 4),
  },
  {
    ...providerBase,
    provider_id: 'p-worth',
    provider: 'Worth it plan',
    cost: null,
    subscription: { cost: money('10'), usage_equivalent_cost: money('25.004', 3), multiple: '2.5004' },
    allocated_cost: money('10'),
  },
  {
    ...providerBase,
    provider_id: 'p-partial',
    provider: 'Partly priced plan',
    cost: null,
    subscription: { cost: money('10'), usage_equivalent_cost: money('5', 1, 2), multiple: '0.5' },
    allocated_cost: money('10'),
  },
  {
    ...providerBase,
    provider_id: 'p-idle',
    provider: 'Idle plan',
    cost: null,
    subscription: { cost: money('0'), usage_equivalent_cost: money('0'), multiple: null },
    allocated_cost: money('0'),
  },
  {
    ...providerBase,
    provider_id: 'p-unknown',
    provider: 'Unpriced service',
    cost: money('0', 0, 3),
    subscription: null,
    allocated_cost: money('0', 0, 3),
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
            recovered_request_count: 0,
            recovery_known_request_count: 50,
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

test('provider table carries one cost column and no other provider cost columns', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/stats')
  const table = page.getByRole('table', { name: 'Providers' })
  const expected = [
    ['Untracked service', '—'],
    ['Metered service', '$12.35'],
    ['Worth it plan', '$10.00'],
    ['Partly priced plan', '$10.00'],
    ['Idle plan', '$0.00'],
    ['Unpriced service', 'Unknown'],
  ]
  await expect(table.getByRole('row').filter({ hasText: 'Metered service' })).toBeVisible()
  const column = (await table.getByRole('columnheader').allInnerTexts()).findIndex((text) => text.trim() === 'Cost')
  expect(column).toBeGreaterThan(-1)
  for (const [name, shown] of expected) {
    const row = table.getByRole('row').filter({ hasText: name })
    await expect(row.getByRole('cell').nth(column)).toHaveText(shown)
  }
  for (const header of ['Estimated cost', 'Subscription cost', 'Value multiple']) {
    await expect(table.getByRole('columnheader', { name: header })).toHaveCount(0)
  }
})

test('providers can be sorted by cost', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/stats')
  const table = page.getByRole('table', { name: 'Providers' })
  const header = table.getByRole('columnheader').filter({ hasText: /^\s*Cost\s*$/ })
  await header.getByRole('button').click()
  if ((await header.getAttribute('aria-sort')) !== 'descending') await header.getByRole('button').click()
  await expect(header).toHaveAttribute('aria-sort', 'descending')
  const names = await table
    .getByRole('row')
    .evaluateAll((rows) => rows.slice(1).map((row) => row.querySelector('td')?.textContent?.trim()))
  expect(names.indexOf('Metered service')).toBeLessThan(names.indexOf('Worth it plan'))
  expect(names.indexOf('Worth it plan')).toBeLessThan(names.indexOf('Idle plan'))
})

test('overview shows only the total cost', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/stats')
  const metric = page
    .locator('.route-metric-strip__item')
    .filter({ has: page.getByText('Total cost', { exact: true }) })
  await expect(metric.locator('p').nth(1)).toHaveText('$22.35')
  await expect(metric).not.toContainText('Subscription value')
  await expect(page.getByRole('heading', { name: 'Cost trend' })).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Model cost and billing' })).toHaveCount(0)
})
