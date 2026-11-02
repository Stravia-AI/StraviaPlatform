import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

const CURRENCY = { kind: 'usd' as const, symbol: '$' }
const TIMESTAMP = '2026-01-01T00:00:00Z'
const DAY_MS = 86_400_000
const money = (amount: string, known = 0, unknown = 0) => ({ amount, known_count: known, unknown_count: unknown })

interface CostPlan {
  id: string
  provider_id: string
  effective_from: number
  basis: 'none' | 'usage' | 'subscription'
  period: 'month' | 'year' | null
  price: { amount: string; currency: 'usd' | 'custom' } | null
  created_at: string
}

interface Mock {
  /** 按时间倒序，首项为当前生效。 */
  plans?: CostPlan[]
  posts?: unknown[]
  /** 非空时 POST 失败并返回该信息。 */
  postFailure?: string
  /** 额度快照；缺省时 404，表示该供应商没有额度读取。 */
  allowance?: Record<string, unknown>
  stats?: unknown[]
  statsSpans?: number[]
  customCurrency?: { symbol: string; usd_rate: string } | null
}

function plan(index: number, overrides: Partial<CostPlan> = {}): CostPlan {
  return {
    id: `plan-${index}`,
    provider_id: 'cost-provider',
    effective_from: Date.UTC(2026, 5, 1) + index * DAY_MS,
    basis: 'usage',
    period: null,
    price: null,
    created_at: TIMESTAMP,
    ...overrides,
  }
}

function snapshot(billing: Record<string, unknown> | undefined, planLabel = 'Max 5x') {
  return {
    provider_id: 'cost-provider',
    guard_supported: false,
    missing_guarded_keys: [],
    suspension: null,
    provider_name: 'Cost provider',
    catalog_provider_id: 'openai-codex',
    channel: 'codex',
    plan_label: planLabel,
    ...(billing ? { billing } : {}),
    status: 'fresh',
    fetched_at: TIMESTAMP,
    allowances: [],
    models: [],
  }
}

function statsRow(overrides: Record<string, unknown> = {}) {
  return {
    provider_id: 'cost-provider',
    deleted: false,
    provider: 'Cost provider',
    upstream_request_count: 10,
    upstream_error_count: 0,
    avg_duration_ms: 100,
    avg_output_tps: 10,
    currency: CURRENCY,
    cost: null,
    subscription: null,
    ...overrides,
  }
}

async function mockCostProvider(page: Page, mock: Mock = {}, vendor: 'openai' | 'openai-codex' = 'openai-codex') {
  const state = { plans: mock.plans ?? [] }
  await page.route('**/api/v1/currency', (route) =>
    route.fulfill({ json: { data: { custom: mock.customCurrency ?? null } } }),
  )
  await page.route('**/api/v1/stats/providers**', async (route) => {
    const url = new URL(route.request().url())
    const start = Number(url.searchParams.get('start_at'))
    const end = Number(url.searchParams.get('end_at'))
    mock.statsSpans?.push(end - start)
    await route.fulfill({ json: { data: mock.stats ?? [] } })
  })
  await page.route('**/api/v1/provider-allowances/cost-provider', async (route) => {
    if (mock.allowance) await route.fulfill({ json: { data: mock.allowance } })
    else await route.fulfill({ status: 404, json: { error: 'not found' } })
  })
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({
        json: {
          data: [
            {
              id: 'cost-provider',
              name: 'Cost provider',
              vendor,
              channel: vendor === 'openai' ? 'default' : 'codex',
              protocol: 'openai-compatible',
              base_url: 'https://fixture.invalid/v1',
              use_proxy: false,
              vendor_options: {},
              configured_credential_fields: ['api_key'],
              is_enabled: true,
              created_at: TIMESTAMP,
              updated_at: TIMESTAMP,
            },
          ],
        },
      })
    } else if (path === '/providers/cost-provider/cost-plans' && request.method() === 'GET') {
      await route.fulfill({ json: { data: state.plans } })
    } else if (path === '/providers/cost-provider/cost-plans' && request.method() === 'POST') {
      const body = request.postDataJSON() as Pick<CostPlan, 'basis'> & Partial<CostPlan>
      mock.posts?.push(body)
      if (mock.postFailure) {
        await route.fulfill({ status: 500, json: { error: mock.postFailure } })
        return
      }
      const created = plan(100 + state.plans.length, {
        basis: body.basis,
        period: body.period ?? null,
        price: body.price ?? null,
        effective_from: Date.now(),
      })
      state.plans = [created, ...state.plans]
      await route.fulfill({ json: { data: created } })
    } else {
      await route.fallback()
    }
  })
}

const subscriptionPlan = (index = 1) =>
  plan(index, { basis: 'subscription', period: 'month', price: { amount: '20', currency: 'usd' } })

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
  await page.setViewportSize({ width: 1440, height: 900 })
})

test('cost is its own tab beside the other provider tabs and defaults to usage-based', async ({ page }) => {
  await mockCostProvider(page)
  await page.goto('/providers/cost-provider?view=connection')

  const tabs = page.getByRole('navigation', { name: 'Provider details' })
  await expect(tabs.getByRole('link')).toHaveText([/Connection settings/, /Upstream models/, /Linked models/, /Cost/])
  await tabs.getByRole('link', { name: 'Cost' }).click()
  await expect(page).toHaveURL(/view=cost/)

  await expect(page.getByRole('heading', { name: 'Cost basis' })).toBeVisible()
  await expect(page.getByText('Usage-based', { exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Usage value' })).toBeVisible()
  await expect(page.getByText('No cost basis changes recorded.')).toBeVisible()
  // 没有价格记录与建议时不出现订阅价格行。
  await expect(page.getByText('Subscription price')).toHaveCount(0)
})

test('editing the basis in the Sheet appends one subscription record and closes with a toast', async ({ page }) => {
  const posts: unknown[] = []
  await mockCostProvider(page, { posts })
  await page.goto('/providers/cost-provider?view=cost')

  await page.getByRole('button', { name: 'Edit Mode' }).click()
  const sheet = page.getByRole('dialog', { name: 'Edit cost basis' })
  const save = sheet.getByRole('button', { name: 'Save', exact: true })
  // 与当前记录相同：没有可保存的内容。
  await expect(save).toBeDisabled()

  await sheet.getByRole('tab', { name: 'Subscription-based' }).click()
  await sheet.getByRole('tab', { name: 'Yearly' }).click()
  await sheet.getByLabel('Subscription price').fill('120.50')
  await expect(save).toBeEnabled()
  await save.click()

  await expect.poll(() => posts.length).toBe(1)
  expect(posts[0]).toEqual({ basis: 'subscription', period: 'year', price: { amount: '120.50', currency: 'usd' } })
  await expect(sheet).toBeHidden()
  await expect(page.getByText('Cost basis saved')).toBeVisible()

  const history = page.getByRole('list', { name: 'Cost basis change history' })
  await expect(history.getByRole('listitem')).toHaveCount(1)
  await expect(history).toContainText('Subscription-based · Yearly')
  await expect(history).toContainText('Current')
  await expect(page.getByText('Subscription price')).toBeVisible()
})

test('an invalid subscription price blocks saving and names the problem', async ({ page }) => {
  const posts: unknown[] = []
  await mockCostProvider(page, { posts })
  await page.goto('/providers/cost-provider?view=cost')

  await page.getByRole('button', { name: 'Edit Mode' }).click()
  const sheet = page.getByRole('dialog', { name: 'Edit cost basis' })
  await sheet.getByRole('tab', { name: 'Subscription-based' }).click()
  const price = sheet.getByLabel('Subscription price')
  await price.fill('abc')
  await sheet.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(sheet.getByText('Enter a non-negative decimal amount.')).toBeVisible()
  expect(posts).toEqual([])

  await price.fill('10')
  await expect(sheet.getByText('Enter a non-negative decimal amount.')).toHaveCount(0)
})

test('a failed append keeps the Sheet and the typed price, and the history unchanged', async ({ page }) => {
  const posts: unknown[] = []
  await mockCostProvider(page, { posts, postFailure: 'cost plan write failed' })
  await page.goto('/providers/cost-provider?view=cost')

  await page.getByRole('button', { name: 'Edit Mode' }).click()
  const sheet = page.getByRole('dialog', { name: 'Edit cost basis' })
  await sheet.getByRole('tab', { name: 'Subscription-based' }).click()
  await sheet.getByLabel('Subscription price').fill('9.99')
  await sheet.getByRole('button', { name: 'Save', exact: true }).click()

  await expect.poll(() => posts.length).toBe(1)
  await expect(sheet.getByRole('alert').filter({ hasText: 'cost plan write failed' })).toBeVisible()
  await expect(sheet.getByLabel('Subscription price')).toHaveValue('9.99')
  await expect(sheet.getByRole('button', { name: 'Save', exact: true })).toBeEnabled()
  await expect(page.getByText('No cost basis changes recorded.')).toBeVisible()
})

test('change history shows the latest five records and says it is truncated', async ({ page }) => {
  // 7 条记录，最新在前；第 6、7 条不显示。
  const plans = [7, 6, 5, 4, 3, 2, 1].map((index) =>
    index % 2 ? plan(index, { basis: 'usage' }) : plan(index, { basis: 'none' }),
  )
  await mockCostProvider(page, { plans })
  await page.goto('/providers/cost-provider?view=cost')

  const history = page.getByRole('list', { name: 'Cost basis change history' })
  await expect(history.getByRole('listitem')).toHaveCount(5)
  await expect(history.getByRole('listitem').first()).toContainText('Current')
  await expect(history.getByText('Current')).toHaveCount(1)
  await expect(page.getByText('Showing the latest 5 changes.')).toBeVisible()
})

test('usage-based providers show their estimated cost for the chosen window', async ({ page }) => {
  const statsSpans: number[] = []
  await mockCostProvider(page, { statsSpans, stats: [statsRow({ cost: money('12.345', 4, 2) })] })
  await page.goto('/providers/cost-provider?view=cost')

  const usage = page.getByRole('region', { name: 'Usage value' })
  await expect(usage).toContainText('Estimated cost')
  // 有未计价调用时金额只是下限。
  await expect(usage).toContainText('≥$12.35')
  await expect(usage).toContainText('Priced calls')
  await expect(usage).not.toContainText('Unpriced')
  expect(statsSpans.at(-1)).toBe(DAY_MS)

  await page.getByRole('tablist', { name: 'Time range' }).getByRole('tab', { name: '30 days' }).click()
  await expect.poll(() => statsSpans.at(-1)).toBe(30 * DAY_MS)
})

test('subscription providers show equivalent cost, subscription cost and value multiple', async ({ page }) => {
  await mockCostProvider(page, {
    plans: [subscriptionPlan()],
    stats: [
      statsRow({ subscription: { cost: money('10'), usage_equivalent_cost: money('25.004', 3), multiple: '2.5004' } }),
    ],
  })
  await page.goto('/providers/cost-provider?view=cost')

  const usage = page.getByRole('region', { name: 'Usage value' })
  await expect(usage).toContainText('Equivalent usage cost')
  await expect(usage).toContainText('$25.00')
  await expect(usage).toContainText('Subscription cost')
  await expect(usage).toContainText('$10.00')
  await expect(usage).toContainText('2.50×')
  await expect(page.getByText('Subscription price')).toBeVisible()
  await expect(page.getByText('$20.00 / mo')).toBeVisible()
})

test('a provider without a stats row or tracked cost explains itself instead of showing zeros', async ({ page }) => {
  await mockCostProvider(page, { plans: [plan(1, { basis: 'none' })] })
  await page.goto('/providers/cost-provider?view=cost')
  await expect(page.getByText('Cost is not tracked for this provider.')).toBeVisible()
  await expect(page.getByRole('tablist', { name: 'Time range' })).toHaveCount(0)

  await mockCostProvider(page, { plans: [plan(1, { basis: 'usage' })], stats: [] })
  await page.goto('/providers/cost-provider?view=cost')
  await expect(page.getByText('No usage to measure in the selected time range.')).toBeVisible()
})

test('a detected priced subscription is adopted through the Sheet, never written automatically', async ({ page }) => {
  const posts: unknown[] = []
  await mockCostProvider(page, {
    posts,
    allowance: snapshot({ kind: 'subscription', period: 'month', price: { amount: '100', currency: 'USD' } }),
  })
  await page.goto('/providers/cost-provider?view=cost')

  const banner = page.getByRole('region', { name: 'Detected from allowance' })
  await expect(banner).toContainText('Subscription detected · Max 5x')
  await expect(banner).toContainText('$100.00 / mo')
  expect(posts).toEqual([])

  await banner.getByRole('button', { name: 'Adopt' }).click()
  const sheet = page.getByRole('dialog', { name: 'Edit cost basis' })
  await expect(sheet.getByRole('tab', { name: 'Subscription-based' })).toHaveAttribute('aria-selected', 'true')
  await expect(sheet.getByLabel('Subscription price')).toHaveValue('100')
  expect(posts).toEqual([])
  await sheet.getByRole('button', { name: 'Save', exact: true }).click()

  await expect.poll(() => posts.length).toBe(1)
  expect(posts[0]).toEqual({ basis: 'subscription', period: 'month', price: { amount: '100', currency: 'usd' } })
  // 已按建议的标价记录，不再重复提示。
  await expect(banner).toBeHidden()
})

test('with a custom currency the detected list price is converted at the current rate', async ({ page }) => {
  const posts: unknown[] = []
  await mockCostProvider(page, {
    posts,
    customCurrency: { symbol: '¥', usd_rate: '7.25' },
    allowance: snapshot({ kind: 'subscription', period: 'month', price: { amount: '19.99', currency: 'USD' } }),
  })
  await page.goto('/providers/cost-provider?view=cost')

  const banner = page.getByRole('region', { name: 'Detected from allowance' })
  await expect(banner).toContainText('¥144.93 / mo')
  await expect(banner).not.toContainText('$')
  await banner.getByRole('button', { name: 'Adopt' }).click()

  const sheet = page.getByRole('dialog', { name: 'Edit cost basis' })
  await expect(sheet.getByLabel('Subscription price')).toHaveValue('144.93')
  await expect(sheet.getByText('¥', { exact: true }).first()).toBeVisible()
  await sheet.getByRole('button', { name: 'Save', exact: true }).click()

  await expect.poll(() => posts.length).toBe(1)
  expect(posts[0]).toEqual({ basis: 'subscription', period: 'month', price: { amount: '144.93', currency: 'custom' } })
  await expect(banner).toBeHidden()
})

test('a detected plan without a list price leaves the amount to the user', async ({ page }) => {
  const posts: unknown[] = []
  await mockCostProvider(page, { posts, allowance: snapshot({ kind: 'subscription', period: 'month' }, 'Team') })
  await page.goto('/providers/cost-provider?view=cost')

  const banner = page.getByRole('region', { name: 'Detected from allowance' })
  await expect(banner).toContainText('Subscription detected · Team')
  await expect(banner).not.toContainText('$')
  await banner.getByRole('button', { name: 'Set as subscription' }).click()

  const sheet = page.getByRole('dialog', { name: 'Edit cost basis' })
  const price = sheet.getByLabel('Subscription price')
  await expect(price).toHaveValue('')
  await expect(price).toBeFocused()
  await sheet.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(sheet.getByText('Enter a non-negative decimal amount.')).toBeVisible()
  expect(posts).toEqual([])
})

test('a dismissed suggestion stays hidden and an explicit "not tracked" choice is never second-guessed', async ({
  page,
}) => {
  await mockCostProvider(page, {
    allowance: snapshot({ kind: 'subscription', period: 'month', price: { amount: '100', currency: 'USD' } }),
  })
  await page.goto('/providers/cost-provider?view=cost')
  const banner = page.getByRole('region', { name: 'Detected from allowance' })
  await banner.getByRole('button', { name: 'Dismiss' }).click()
  await expect(banner).toBeHidden()

  await mockCostProvider(page, {
    plans: [plan(1, { basis: 'none' })],
    allowance: snapshot({ kind: 'subscription', period: 'month', price: { amount: '100', currency: 'USD' } }),
  })
  await page.goto('/providers/cost-provider?view=cost')
  await expect(page.getByText('Cost is not tracked for this provider.')).toBeVisible()
  await expect(banner).toHaveCount(0)
})

test('providers without allowance support never request allowance data or show a suggestion', async ({ page }) => {
  const allowanceRequests: string[] = []
  page.on('request', (request) => {
    if (request.url().includes('/provider-allowances/')) allowanceRequests.push(request.url())
  })
  await mockCostProvider(page, { allowance: snapshot({ kind: 'subscription', period: 'month' }) }, 'openai')
  await page.goto('/providers/cost-provider?view=cost')
  await expect(page.getByRole('heading', { name: 'Cost basis' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Detected from allowance' })).toHaveCount(0)
  expect(allowanceRequests).toEqual([])
})

test('the cost tab fits a 390px viewport without horizontal scrolling', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await mockCostProvider(page, {
    plans: [subscriptionPlan()],
    allowance: snapshot({ kind: 'subscription', period: 'year', price: { amount: '200', currency: 'USD' } }, 'Ultra'),
    stats: [
      statsRow({ subscription: { cost: money('10'), usage_equivalent_cost: money('25.004', 3), multiple: '2.5004' } }),
    ],
  })
  await page.goto('/providers/cost-provider?view=cost')
  await expect(page.getByRole('region', { name: 'Detected from allowance' })).toBeVisible()
  await expect(page.getByRole('region', { name: 'Usage value' })).toContainText('2.50×')
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})
