import { expect, test, type Locator, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

const QUARTER = 900_000
const HOUR = 3_600_000
const END = Date.UTC(2026, 8, 17, 8, 15)
// 悬停目标：构成里只有输入 40 和输出 10，其余分类未上报。
const SPECIAL = END - 4 * QUARTER
const CURRENCY = { kind: 'usd' as const, symbol: '$' }
const money = { amount: '0.01', known_count: 1, unknown_count: 0 }

test.use({ timezoneId: 'UTC' })

const PROVIDERS = [
  ['provider-a', 'Alpha'],
  ['provider-b', 'Beta'],
] as const

type Scope = 'all' | 'provider' | 'api-key'

function scopeOf(url: URL): Scope {
  if (url.searchParams.has('provider_ids')) return 'provider'
  if (url.searchParams.has('api_key_ids')) return 'api-key'
  return 'all'
}

/** 概览输入 Token 随筛选口径变化，用来确认页面真的换成了筛选后的结果。 */
const OVERVIEW_INPUT: Record<Scope, number> = { all: 600, provider: 111, 'api-key': 222 }

async function stubStats(page: Page) {
  await page.clock.setFixedTime(END - 8 * 60_000)
  const requests: URL[] = []
  await page.route('**/api/v1/stats/**', async (route) => {
    const url = new URL(route.request().url())
    requests.push(url)
    const start = Number(url.searchParams.get('start_at'))
    const end = Number(url.searchParams.get('end_at'))
    const scope = scopeOf(url)
    if (url.pathname.endsWith('/overview')) {
      await route.fulfill({
        json: {
          data: {
            total_requests: 128,
            terminal_request_count: 128,
            failed_request_count: 2,
            recovered_request_count: 0,
            recovery_known_request_count: 128,
            upstream_request_count: 128,
            upstream_error_count: 2,
            total_input_tokens: OVERVIEW_INPUT[scope],
            total_output_tokens: 86,
            total_cache_read_tokens: 320,
            total_cache_write_tokens: 12,
            total_reasoning_tokens: null,
            avg_duration_ms: 120,
            avg_first_token_ms: 40,
            avg_output_tps: 25,
            currency: CURRENCY,
            actual_cost: money,
          },
        },
      })
    } else if (url.pathname.endsWith('/series')) {
      const bucket = Number(url.searchParams.get('bucket')) * 1000
      const data = []
      for (let at = start; at < end; at += bucket) {
        const special = at === SPECIAL
        data.push({
          bucket_start: at,
          bucket_ms: bucket,
          request_count: 1,
          terminal_request_count: 1,
          failed_request_count: 0,
          recovered_request_count: 0,
          recovery_known_request_count: 1,
          upstream_request_count: 1,
          upstream_error_count: 0,
          total_input_tokens: special ? 40 : 100,
          total_output_tokens: special ? 10 : 0,
          total_cache_read_tokens: special ? null : 80,
          total_cache_write_tokens: special ? null : 10,
          total_reasoning_tokens: null,
          avg_duration_ms: 120,
          avg_first_token_ms: 40,
          avg_output_tps: 25,
          currency: CURRENCY,
          actual_cost: money,
        })
      }
      await route.fulfill({ json: { data } })
    } else if (url.pathname.endsWith('/models')) {
      const period = start === SPECIAL && end === SPECIAL + QUARTER
      await route.fulfill({
        json: {
          data: [
            {
              model_id: 'm-1',
              model: period ? 'Hovered model' : 'Everyday model',
              deleted: false,
              request_count: period ? 3 : 90,
              total_input_tokens: 1,
              total_output_tokens: 1,
              cache_read_tokens: null,
              cache_write_tokens: null,
              reasoning_tokens: null,
              currency: CURRENCY,
              cost: money,
            },
          ],
        },
      })
    } else if (url.pathname.endsWith('/providers')) {
      await route.fulfill({
        json: {
          data: PROVIDERS.map(([provider_id, provider]) => ({
            provider_id,
            provider,
            deleted: false,
            upstream_request_count: 64,
            upstream_error_count: 1,
            avg_duration_ms: 100,
            avg_output_tps: 10,
            currency: CURRENCY,
            cost: money,
            subscription: null,
          })),
        },
      })
    } else if (url.pathname.endsWith('/api-keys')) {
      await route.fulfill({
        json: {
          data: [
            {
              api_key_id: 'key-a',
              deleted: false,
              api_key_name: 'Desktop client',
              request_count: 64,
              total_input_tokens: 300,
              total_output_tokens: 40,
              cache_read_tokens: 160,
              cache_write_tokens: 6,
              reasoning_tokens: null,
              last_used_at: END - HOUR,
              currency: CURRENCY,
              cost: money,
              billing: money,
            },
          ],
        },
      })
    } else {
      await route.fulfill({ json: { data: [] } })
    }
  })
  return requests
}

const region = (page: Page, name: string) => page.getByRole('region', { name, exact: true })
// 选择只靠点击行：表格没有复选框列，选中态由 aria-selected 暴露。
const tableRow = (page: Page, regionName: string, rowName: string): Locator =>
  region(page, regionName).getByRole('row').filter({ hasText: rowName })
const metric = (page: Page, label: string) =>
  page.locator('.route-metric-strip__item').filter({ has: page.getByText(label, { exact: true }) })

async function openUsage(page: Page) {
  await prepareApp(page)
  const requests = await stubStats(page)
  await page.setViewportSize({ width: 1600, height: 1000 })
  await page.goto('/stats')
  await page.locator('[data-slot="select-trigger"]').click()
  await page.getByRole('option', { name: 'Last 6h', exact: true }).click()
  await expect(metric(page, 'Input Tokens')).toContainText('600')
  await expect(tableRow(page, 'Providers', 'Alpha')).toBeVisible()
  return requests
}

const lastRequest = (requests: URL[], endpoint: string, match: (url: URL) => boolean) =>
  requests.findLast((url) => url.pathname.endsWith(`/${endpoint}`) && match(url))

test('selecting Providers filters the charts and clears API Key selection, and vice versa', async ({ page }) => {
  const requests = await openUsage(page)

  // 不渲染复选框列，也不显示「N of M row(s) selected」页脚。
  await expect(region(page, 'Providers').getByRole('checkbox')).toHaveCount(0)
  await expect(region(page, 'API Key usage').getByRole('checkbox')).toHaveCount(0)
  await expect(page.getByText('row(s) selected')).toHaveCount(0)

  await tableRow(page, 'Providers', 'Alpha').click()
  await expect(tableRow(page, 'Providers', 'Alpha')).toHaveAttribute('aria-selected', 'true')
  await expect(metric(page, 'Input Tokens')).toContainText('111')
  await expect(page.getByRole('status').filter({ hasText: 'Selected Providers: 1' })).toBeVisible()
  expect(
    lastRequest(requests, 'overview', (url) => url.searchParams.has('provider_ids'))?.searchParams.get('provider_ids'),
  ).toBe('provider-a')
  expect(
    requests.some((url) => url.pathname.endsWith('/series') && url.searchParams.get('provider_ids') === 'provider-a'),
  ).toBe(true)
  // 筛选口径没有请求成功率和模型占比，明确说明而不是留空或显示整体数字。
  await expect(page.getByText("Request success rate isn't available for a selection.")).toBeVisible()
  await expect(page.getByText("Model share isn't available for a selection.")).toBeVisible()

  await tableRow(page, 'Providers', 'Beta').click()
  await expect(page.getByRole('status').filter({ hasText: 'Selected Providers: 2' })).toBeVisible()
  await expect
    .poll(() =>
      lastRequest(requests, 'overview', (url) => url.searchParams.has('provider_ids'))?.searchParams.get(
        'provider_ids',
      ),
    )
    .toBe('provider-a,provider-b')

  // 选 API Key 会取消全部 Provider，图表改按 API Key 口径。
  await tableRow(page, 'API Key usage', 'Desktop client').click()
  await expect(tableRow(page, 'Providers', 'Alpha')).toHaveAttribute('aria-selected', 'false')
  await expect(tableRow(page, 'Providers', 'Beta')).toHaveAttribute('aria-selected', 'false')
  await expect(metric(page, 'Input Tokens')).toContainText('222')
  await expect(page.getByRole('status').filter({ hasText: 'Selected API Keys: 1' })).toBeVisible()
  expect(
    lastRequest(requests, 'overview', (url) => url.searchParams.has('api_key_ids'))?.searchParams.get('api_key_ids'),
  ).toBe('key-a')
  expect(requests.every((url) => !(url.searchParams.has('api_key_ids') && url.searchParams.has('provider_ids')))).toBe(
    true,
  )

  // 反过来选 Provider 会取消 API Key。
  await tableRow(page, 'Providers', 'Beta').click()
  await expect(tableRow(page, 'API Key usage', 'Desktop client')).toHaveAttribute('aria-selected', 'false')
  await expect(metric(page, 'Input Tokens')).toContainText('111')

  await page.getByRole('button', { name: 'Clear selection' }).click()
  await expect(tableRow(page, 'Providers', 'Beta')).toHaveAttribute('aria-selected', 'false')
  await expect(metric(page, 'Input Tokens')).toContainText('600')
  await expect(page.getByRole('status').filter({ hasText: 'selected' })).toHaveCount(0)
  await expect(page.getByText("Request success rate isn't available for a selection.")).toHaveCount(0)
})

test('mobile rows select by tap or Enter and expose the pressed state', async ({ page }) => {
  await openUsage(page)
  await page.setViewportSize({ width: 390, height: 900 })
  const alpha = region(page, 'Providers').getByRole('button', { name: /Alpha/ })

  await alpha.click()
  await expect(alpha).toHaveAttribute('aria-pressed', 'true')
  await expect(metric(page, 'Input Tokens')).toContainText('111')
  await expect(region(page, 'Providers').getByRole('checkbox')).toHaveCount(0)

  await alpha.press('Enter')
  await expect(alpha).toHaveAttribute('aria-pressed', 'false')
  await expect(metric(page, 'Input Tokens')).toContainText('600')
})

test('hovering a Token activity cell shows that period in composition and models, and a range ending there in the trends', async ({
  page,
}) => {
  const requests = await openUsage(page)
  const legend = region(page, 'Token breakdown').getByRole('listitem')
  await expect(legend).toHaveCount(4)

  await page.locator(`.token-activity-cell[data-start="${SPECIAL}"]`).hover()
  // 构成只含该格上报的分类；图例名不带 “Token” 后缀。
  await expect(legend).toHaveCount(2)
  await expect(legend.filter({ hasText: 'Input' })).toContainText('80%')
  await expect(legend.filter({ hasText: 'Output' })).toContainText('20%')
  await expect(region(page, 'Token breakdown').getByText('50', { exact: true })).toBeVisible()
  await expect(region(page, 'Token breakdown').getByText('Sep 17 07:15 – 07:30')).toBeVisible()
  await expect(region(page, 'Models').getByText('Hovered model')).toBeVisible()
  await expect(region(page, 'Models').getByText('Sep 17 07:15 – 07:30')).toBeVisible()
  expect(
    requests.some(
      (url) =>
        url.pathname.endsWith('/models') &&
        Number(url.searchParams.get('start_at')) === SPECIAL &&
        Number(url.searchParams.get('end_at')) === SPECIAL + QUARTER,
    ),
  ).toBe(true)
  // 延迟与成功率看与所选窗口等长、止于该格结束的区间。
  const viewEnd = SPECIAL + QUARTER
  const viewStart = viewEnd - 6 * HOUR
  expect(
    requests.some(
      (url) =>
        url.pathname.endsWith('/series') &&
        url.searchParams.get('bucket') === '3600' &&
        Number(url.searchParams.get('start_at')) === viewStart &&
        Number(url.searchParams.get('end_at')) === viewEnd,
    ),
  ).toBe(true)
  // 汇总条和 Token 活动图始终是所选窗口，不跟悬停漂移。
  await expect(metric(page, 'Input Tokens')).toContainText('600')

  await page.mouse.move(5, 5)
  await expect(legend).toHaveCount(4)
  await expect(region(page, 'Models').getByText('Everyday model')).toBeVisible()
  await expect(region(page, 'Token breakdown').getByText('Sep 17 07:15 – 07:30')).toHaveCount(0)
})

test('hovering a legend item enlarges its slice, dims the others and shows its value in the centre', async ({
  page,
}) => {
  await openUsage(page)
  const breakdown = region(page, 'Token breakdown')
  const slice = (key: string) => breakdown.locator(`path[data-slice="${key}"]`)
  const area = async (key: string) => {
    const box = (await slice(key).boundingBox())!
    return box.width * box.height
  }

  const before = await area('cache-read')
  await breakdown.getByRole('listitem').filter({ hasText: 'Cache read' }).hover()
  await expect(slice('cache-read')).toBeVisible()
  // 悬停项向外伸出，其余分项变淡。
  await expect.poll(() => area('cache-read')).toBeGreaterThan(before)
  await expect(slice('input')).toHaveCSS('opacity', /^0\.[3-6]/)
  await expect(breakdown.getByText('320', { exact: true })).toBeVisible()
  expect(await slice('cache-read').getAttribute('opacity')).toBe('1')

  await page.mouse.move(5, 5)
  await expect.poll(() => area('cache-read')).toBeLessThanOrEqual(before + 0.5)
  await expect(breakdown.getByText('1K', { exact: true })).toBeVisible()
})
