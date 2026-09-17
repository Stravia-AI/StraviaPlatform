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
      reset_at: 1788883200000,
      condition: 'normal',
      forecast: { status: 'no_risk', projected_remaining_percent: 24.5 },
    },
    {
      key: 'credits_balance',
      label: 'Credit balance',
      kind: 'balance',
      remaining: { value: 0, unit: 'currency', currency: 'USD' },
      condition: 'exhausted',
      forecast: { status: 'unknown' },
    },
    {
      key: 'credits_balance_cny',
      label: 'Credit balance',
      kind: 'balance',
      remaining: { value: 9.99, unit: 'currency', currency: 'CNY' },
      condition: 'normal',
      forecast: { status: 'unknown' },
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
          condition: 'tight',
          forecast: { status: 'will_exhaust', exhausts_at: 1788200000000 },
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
      reset_at: 1788796800000,
      condition: 'exhausted',
      forecast: { status: 'will_exhaust', projected_remaining_percent: 0, exhausts_at: 1788700000000 },
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
    const providerId = path.match(/provider-allowances\/([^/]+?)(?:\/refresh)?$/)?.[1]
    if (providerId) {
      await route.fulfill({ json: { data: snapshots.find((snapshot) => snapshot.provider_id === providerId) ?? null } })
      return
    }
    await route.fulfill({
      json: {
        data: snapshots.map((snapshot) => ({
          provider_id: snapshot.provider_id,
          provider_name: snapshot.provider_name,
          catalog_provider_id: snapshot.catalog_provider_id,
          channel: snapshot.channel,
          snapshot,
          refreshing: false,
        })),
      },
    })
  })
  return posts
}

async function selectFilter(page: Page, label: string, option: string): Promise<void> {
  await page.getByRole('button', { name: label, exact: true }).click()
  await page.getByRole('option', { name: option, exact: true }).click()
}

test('renders the matrix, shared summary, timeline, forecast, model details, and refresh actions', async ({ page }) => {
  const posts = await mockAllowances(page, [errorSnapshot, staleSnapshot, freshSnapshot])
  await page.goto('/allowances')

  await expect(page.getByRole('heading', { name: 'Allowance overview' })).toBeVisible()
  const matrix = page.getByRole('table', { name: 'Allowance matrix' })
  await expect(matrix).toBeVisible()
  await expect(matrix.getByText('Alpha account')).toBeVisible()
  await expect(matrix.getByText('Beta account')).toBeVisible()
  await expect(matrix.getByText('Gamma account')).toBeVisible()
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(2)
  await expect(matrix.getByText('Fresh', { exact: true })).toBeVisible()
  await expect(matrix.getByText('Stale', { exact: true })).toBeVisible()
  await expect(matrix.getByText('Unavailable', { exact: true })).toBeVisible()
  await expect(matrix.getByText('Exhausted', { exact: true }).first()).toBeVisible()
  await expect(matrix.getByText('0 USD')).toBeVisible()
  // 币种后缀 key（credits_balance_cny）与普通 credits_balance 共用同一标签展示
  await expect(matrix.getByText('9.99 CNY')).toBeVisible()
  await expect(matrix.getByText('Credit balance', { exact: true })).toHaveCount(2)
  await expect(matrix.getByText('Showing the last successful result because this refresh failed.')).toBeVisible()
  await expect(matrix.getByText('Reconnect this model service or update its credential.')).toBeVisible()

  const conditionSummary = page.getByRole('region', { name: 'Allowance condition' })
  const forecastPanel = page
    .locator('[data-slot="card"]')
    .filter({ has: page.getByRole('heading', { name: 'Exhaustion forecast' }) })
  await expect(conditionSummary.getByText('Exhausted', { exact: true })).toBeVisible()
  await expect(conditionSummary.getByText(/Lowest remaining/)).toBeVisible()
  const emptyWindows = page.getByTestId('allowance-empty-windows')
  await expect(emptyWindows).toContainText('Beta account')
  await expect(emptyWindows).toContainText('Weekly window')
  await expect(emptyWindows).not.toContainText('Alpha account')
  await expect(matrix.getByTestId('allowance-provider-provider-beta')).toContainText('Weekly window exhausted')
  await expect(matrix.getByTestId('allowance-provider-provider-alpha')).not.toContainText('Weekly window exhausted')
  await expect(page.getByRole('heading', { name: 'Reset timeline' })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Exhaustion forecast' })).toBeVisible()
  await expect(page.getByText('Based on the current window')).toBeVisible()
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 1')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 1')
  await expect(forecastPanel).toContainText('exhausted at')
  await expect(forecastPanel).not.toContainText('may exhaust')

  await matrix.getByLabel('Show model allowances for Alpha account').click()
  await matrix.getByText('claude-opus-4-6').click()
  await expect(matrix.getByText(/Resets/)).toBeVisible()

  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-beta/refresh')
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-gamma/refresh')
  await matrix.getByRole('button', { name: 'Refresh Alpha account' }).click()
  await expect
    .poll(() => posts.filter((path) => path === '/api/v1/provider-allowances/provider-alpha/refresh').length)
    .toBe(2)
})

test('renders provider shells first and fills each group as its snapshot arrives', async ({ page }) => {
  let releaseSnapshot!: () => void
  const gate = new Promise<void>((resolve) => (releaseSnapshot = resolve))
  await page.route('**/api/v1/provider-allowances**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname
    const providerId = path.match(/provider-allowances\/([^/]+)$/)?.[1]
    if (providerId && providerId !== 'refresh' && request.method() === 'GET') {
      await gate
      await route.fulfill({ json: { data: freshSnapshot } })
      return
    }
    await route.fulfill({
      json: {
        data: [
          {
            provider_id: freshSnapshot.provider_id,
            provider_name: freshSnapshot.provider_name,
            catalog_provider_id: freshSnapshot.catalog_provider_id,
            channel: freshSnapshot.channel,
            refreshing: true,
          },
        ],
      },
    })
  })
  await page.goto('/allowances')

  const matrix = page.getByRole('table', { name: 'Allowance matrix' })
  const conditionSummary = page.getByRole('region', { name: 'Allowance condition' })
  // 组头先行渲染；快照到达前行区与聚合区各自转圈，不出全屏骨架
  await expect(matrix.getByText('Alpha account')).toBeVisible()
  await expect(matrix.getByTestId('allowance-loading-provider-alpha')).toBeVisible()
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(0)
  await expect(conditionSummary.getByText('Normal', { exact: true })).toHaveCount(0)

  releaseSnapshot()
  await expect(matrix.getByText('Weekly window', { exact: true })).toBeVisible()
  await expect(matrix.getByTestId('allowance-loading-provider-alpha')).toHaveCount(0)
  await expect(conditionSummary.getByText('Normal', { exact: true })).toBeVisible()
})

test('keeps multiple model allowances open and distinguishes unknown utilization from zero', async ({ page }) => {
  await mockAllowances(page, [
    {
      ...freshSnapshot,
      models: [
        freshSnapshot.models[0],
        {
          model: 'claude-sonnet-4-6',
          allowances: [{ ...freshSnapshot.models[0].allowances[0], label: 'Sonnet window' }],
        },
      ],
    },
  ])
  await page.goto('/allowances')
  const matrix = page.getByRole('table', { name: 'Allowance matrix' })
  await matrix.getByRole('button', { name: 'Show model allowances for Alpha account' }).click()
  const opus = matrix.getByRole('button', { name: 'claude-opus-4-6', exact: true })
  const sonnet = matrix.getByRole('button', { name: 'claude-sonnet-4-6', exact: true })
  await opus.click()
  await sonnet.click()
  await expect(opus).toHaveAttribute('aria-expanded', 'true')
  await expect(sonnet).toHaveAttribute('aria-expanded', 'true')
  await expect(matrix.getByText('Sonnet window')).toBeVisible()
  await sonnet.click()
  await expect(opus).toHaveAttribute('aria-expanded', 'true')
  await expect(matrix.getByRole('progressbar', { name: 'Weekly window Utilization' })).toHaveAttribute(
    'aria-valuenow',
    '55.625',
  )
  // 两条余额行（USD 耗尽 + CNY 充值）均无利用率，进度条都应为不确定态
  await expect(matrix.getByRole('progressbar', { name: 'Credit balance Utilization' })).toHaveCount(2)
  for (const bar of await matrix.getByRole('progressbar', { name: 'Credit balance Utilization' }).all()) {
    await expect(bar).not.toHaveAttribute('aria-valuenow')
  }

  await page.setViewportSize({ width: 375, height: 760 })
  await expect(
    page.getByRole('progressbar', { name: 'Weekly window Utilization', includeHidden: false }),
  ).toHaveAttribute('aria-valuenow', '55.625')
  for (const bar of await page
    .getByRole('progressbar', { name: 'Credit balance Utilization', includeHidden: false })
    .all()) {
    await expect(bar).not.toHaveAttribute('aria-valuenow')
  }
})

test('keeps allowance details visible when refresh replaces provider snapshots', async ({ page }) => {
  const snapshots = structuredClone([freshSnapshot, staleSnapshot])
  const posts = await mockAllowances(page, snapshots)
  await page.goto('/allowances')
  const matrix = page.getByRole('table', { name: 'Allowance matrix' })
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(2)
  await expect(matrix.getByText('0 USD')).toBeVisible()

  snapshots[0].allowances[1].remaining!.value = 12
  await page.getByRole('button', { name: 'Refresh all' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect(matrix.getByText('12 USD')).toBeVisible()
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(2)

  snapshots[0].allowances[1].remaining!.value = 24
  await matrix.getByRole('button', { name: 'Refresh Alpha account' }).click()
  await expect.poll(() => posts).toContain('/api/v1/provider-allowances/provider-alpha/refresh')
  await expect(matrix.getByText('24 USD')).toBeVisible()
  await expect(matrix.getByText('Weekly window', { exact: true })).toHaveCount(2)
})

test('does not treat an exhausted allowance without a reset date as exhausted', async ({ page }) => {
  await mockAllowances(page, [freshSnapshot])
  await page.goto('/allowances')

  const matrix = page.getByRole('table', { name: 'Allowance matrix' })
  const provider = matrix.getByTestId('allowance-provider-provider-alpha')
  const forecastPanel = page
    .locator('[data-slot="card"]')
    .filter({ has: page.getByRole('heading', { name: 'Exhaustion forecast' }) })
  await expect(matrix.getByText('0 USD')).toBeVisible()
  await expect(provider.getByText('Exhausted', { exact: true })).toHaveCount(0)
  await expect(
    page.getByRole('region', { name: 'Allowance condition' }).getByText('Normal', { exact: true }),
  ).toBeVisible()
  await expect(page.getByTestId('allowance-empty-windows')).toHaveCount(0)
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 0')
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 1')

  await selectFilter(page, 'Filter by allowance condition', 'Exhausted')
  await expect(page.getByText('No allowances match these filters.')).toBeVisible()
})

test('search and all filters drive the same visible collection', async ({ page }) => {
  await mockAllowances(page, [errorSnapshot, staleSnapshot, freshSnapshot])
  await page.goto('/allowances')

  const search = page.getByLabel('Search model services')
  const timelinePanel = page
    .locator('[data-slot="card"]')
    .filter({ has: page.getByRole('heading', { name: 'Reset timeline' }) })
  const forecastPanel = page
    .locator('[data-slot="card"]')
    .filter({ has: page.getByRole('heading', { name: 'Exhaustion forecast' }) })
  await search.fill('alpha')
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Alpha account')).toBeVisible()
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Beta account')).toHaveCount(0)
  await expect(timelinePanel).not.toContainText('Beta account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 0')
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 1')

  await search.fill('Weekly')
  await expect(page.getByText('No allowances match these filters.')).toBeVisible()
  await search.fill('')

  await selectFilter(page, 'Filter by service type', 'openai / codex')
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Beta account')).toBeVisible()
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Alpha account')).toHaveCount(0)
  await expect(timelinePanel).toContainText('Beta account')
  await expect(timelinePanel).not.toContainText('Alpha account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 1')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )
  await expect(forecastPanel).toContainText('Current windows are already exhausted')

  await selectFilter(page, 'Filter by service type', 'All')
  await selectFilter(page, 'Filter by allowance condition', 'Exhausted')
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Alpha account')).toHaveCount(0)
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Beta account')).toBeVisible()
  await expect(timelinePanel).toContainText('Beta account')
  await expect(timelinePanel).not.toContainText('Alpha account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 1')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )

  await selectFilter(page, 'Filter by allowance condition', 'All')
  await selectFilter(page, 'Filter by data freshness', 'Unavailable')
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Gamma account')).toBeVisible()
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Alpha account')).toHaveCount(0)
  await expect(timelinePanel).not.toContainText('Alpha account')
  await expect(timelinePanel).not.toContainText('Beta account')
  await expect(forecastPanel.getByTestId('allowance-forecast-exhausted')).toHaveAttribute('aria-label', 'Exhausted 0')
  await expect(forecastPanel.getByTestId('allowance-forecast-will-exhaust')).toHaveAttribute(
    'aria-label',
    'Will exhaust 0',
  )
  await expect(forecastPanel.getByTestId('allowance-forecast-no-risk')).toHaveAttribute('aria-label', 'No risk 0')
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
  await page.unroute('**/api/v1/provider-allowances**')
  await mockAllowances(page, [freshSnapshot])
  await page.getByRole('button', { name: 'Retry' }).click()
  await expect(page.getByRole('table', { name: 'Allowance matrix' }).getByText('Alpha account')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Retry' })).toHaveCount(0)
})

test('keeps the matrix and side panels usable on a narrow Chinese viewport', async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 760 })
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
  await mockAllowances(page, [freshSnapshot])
  await page.goto('/allowances')

  await expect(page.getByRole('heading', { name: '额度总览' })).toBeVisible()
  await expect(page.getByRole('button', { name: '全部刷新' })).toBeVisible()
  const matrixCard = page.locator('[data-slot="card"]').filter({ has: page.getByRole('heading', { name: '额度矩阵' }) })
  const desktopMatrix = matrixCard.locator('.route-desktop-table')
  const mobileMatrix = matrixCard.locator('.route-mobile-list')
  await expect(desktopMatrix).toHaveCount(1)
  await expect(desktopMatrix).toBeHidden()
  await expect(mobileMatrix).toHaveCount(1)
  await expect(mobileMatrix).toBeVisible()
  await expect(mobileMatrix.getByText('Alpha account')).toBeVisible()
  await expect(page.getByRole('heading', { name: '重置时间轴' })).toBeVisible()
  await expect(page.getByRole('heading', { name: '预计耗尽' })).toBeVisible()
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  )
  expect(overflow).toBeLessThanOrEqual(0)
})
