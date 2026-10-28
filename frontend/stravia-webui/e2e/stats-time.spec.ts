import { expect, test, type Locator, type Page, type Route } from '@playwright/test'

import { prepareApp } from './prepare-app'

const HOUR = 3_600_000
const NOW = Date.UTC(2026, 9, 11, 12, 7)
const LIVE_END = Date.UTC(2026, 9, 11, 12, 15)
const overview = (requests = 12) => ({
  total_requests: requests,
  terminal_request_count: requests,
  failed_request_count: 0,
  upstream_request_count: requests,
  upstream_error_count: 0,
  total_input_tokens: 600,
  total_output_tokens: 86,
  total_cache_read_tokens: 320,
  total_cache_write_tokens: 12,
  total_reasoning_tokens: null,
  avg_duration_ms: 120,
  avg_first_token_ms: 40,
  avg_output_tps: 25,
})

type Window = { start: number; end: number }

async function fixture(page: Page, intercept?: (route: Route, url: URL) => Promise<boolean>) {
  await prepareApp(page)
  const requests: URL[] = []
  await page.route('**/api/v1/stats/**', async (route) => {
    const url = new URL(route.request().url())
    requests.push(url)
    if (intercept && (await intercept(route, url))) return
    const endpoint = url.pathname.split('/').at(-1)
    if (endpoint === 'overview') {
      await route.fulfill({ json: { data: overview() } })
    } else if (endpoint === 'series') {
      const bucketMs = Number(url.searchParams.get('bucket')) * 1000
      await route.fulfill({
        json: {
          data: [
            {
              bucket_ms: bucketMs,
              bucket_start: Number(url.searchParams.get('start_at')),
              request_count: 12,
              terminal_request_count: 12,
              failed_request_count: 0,
              upstream_request_count: 12,
              upstream_error_count: 0,
              total_input_tokens: 600,
              total_output_tokens: 86,
              total_cache_read_tokens: 320,
              total_cache_write_tokens: 12,
              total_reasoning_tokens: null,
              avg_duration_ms: 120,
              avg_first_token_ms: 40,
              avg_output_tps: 25,
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

async function expectWindow(requests: URL[], window: Window) {
  await expect
    .poll(() => {
      const matching = requests.filter(
        (url) =>
          Number(url.searchParams.get('start_at')) === window.start &&
          Number(url.searchParams.get('end_at')) === window.end,
      )
      return [...new Set(matching.map((url) => url.pathname.split('/').at(-1)))].sort()
    })
    .toEqual(['api-keys', 'models', 'overview', 'providers', 'series'])
  for (const url of requests) {
    expect(url.searchParams.has('start_at')).toBe(true)
    expect(url.searchParams.has('end_at')).toBe(true)
    expect(url.searchParams.has('hours')).toBe(false)
    expect(url.searchParams.has('tz_offset')).toBe(false)
    expect(Number(url.searchParams.get('start_at')) % 900_000).toBe(0)
    expect(Number(url.searchParams.get('end_at')) % 900_000).toBe(0)
  }
}

async function chooseRange(page: Page, start: string, end: string) {
  await page.getByRole('button', { name: 'Date and time range', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Start', { exact: true }).fill(start)
  await dialog.getByLabel('End', { exact: true }).fill(end)
  return dialog
}

test.describe('statistics absolute time windows', () => {
  test.use({ timezoneId: 'UTC' })

  test('the preset select keeps its chevron inside the control for the longest Chinese label', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await fixture(page)
    await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
    await page.goto('/stats')
    const trigger = page.locator('[data-slot="select-trigger"]')
    await expect(trigger).toContainText('最近 24 小时')
    expect(await trigger.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0)
    expect(await trigger.locator('span').evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0)
  })

  test('selected views retain each preset while activity independently queries earlier history', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    const requests = await fixture(page)
    await page.goto('/stats')
    for (const [label, hours, bucket] of [
      ['Last 6h', 6, '900'],
      ['Last 24h', 24, '3600'],
      ['Last 3d', 72, '21600'],
      ['Last 7d', 168, '86400'],
    ] as const) {
      await page.locator('[data-slot="select-trigger"]').click()
      await page.getByRole('option', { name: label, exact: true }).click()
      const window = { start: LIVE_END - hours * HOUR, end: LIVE_END }
      await expectWindow(requests, window)
      const series = requests.filter(
        (url) => url.pathname.endsWith('/series') && Number(url.searchParams.get('end_at')) === window.end,
      )
      await expect
        .poll(() =>
          requests.some(
            (url) =>
              url.pathname.endsWith('/series') &&
              Number(url.searchParams.get('end_at')) === window.end &&
              Number(url.searchParams.get('start_at')) <= window.start &&
              url.searchParams.get('bucket') === bucket,
          ),
        )
        .toBe(true)
      expect(
        series.some(
          (url) =>
            url.searchParams.get('bucket') === '3600' && Number(url.searchParams.get('start_at')) === window.start,
        ),
      ).toBe(true)
    }
  })

  test('previous and next move a fixed span, refresh does not drift it, and return live advances', async ({ page }) => {
    // install() starts a running clock; pause at a later instant before the app creates timers.
    await page.clock.install({ time: NOW - HOUR })
    await page.clock.pauseAt(NOW)
    const requests = await fixture(page)
    const selectWindow = async (
      window: Window,
      action: () => Promise<unknown>,
      cachedGeneration?: URL[],
      live = false,
    ) => {
      const before = requests.length
      // Latency owns the selected interval even when activity uses the same grain.
      // Register selected-window response boundaries before acting; activity is checked separately.
      const responses = (cachedGeneration ? [] : ['overview', 'series', 'models', 'providers', 'api-keys']).map(
        (endpoint) =>
          page.waitForResponse((response) => {
            const url = new URL(response.url())
            return (
              url.pathname === `/api/v1/stats/${endpoint}` &&
              Number(url.searchParams.get('start_at')) === window.start &&
              Number(url.searchParams.get('end_at')) === window.end &&
              (endpoint !== 'series' || url.searchParams.get('bucket') === '3600')
            )
          }),
      )
      await action()
      for (const response of await Promise.all(responses)) {
        expect(response.ok()).toBe(true)
        expect(await response.finished()).toBeNull()
      }
      const currentRequests = requests.slice(before)
      const generation = cachedGeneration ?? currentRequests
      await expectWindow(generation, window)
      for (const url of [...generation, ...currentRequests]) {
        if (url.pathname.endsWith('/series')) {
          expect(Number(url.searchParams.get('start_at'))).toBeLessThanOrEqual(window.start)
        } else {
          expect(Number(url.searchParams.get('start_at'))).toBe(window.start)
        }
        expect(Number(url.searchParams.get('end_at'))).toBe(window.end)
      }
      // The retained calendar control shows the selected range without opening
      // a dialog whose close transition cannot finish while the clock is paused.
      const visibleTime = (value: number) => {
        const date = new Date(value)
        return `${date.getUTCMonth() + 1}/${date.getUTCDate()}/${date.getUTCFullYear()} ${date.toISOString().slice(11, 19)}`
      }
      await expect(page.getByRole('button', { name: 'Date and time range', exact: true })).toContainText(
        `${visibleTime(window.start)} — ${live ? 'Live' : visibleTime(window.end)}`,
      )
      return generation
    }
    await selectWindow({ start: LIVE_END - 24 * HOUR, end: LIVE_END }, () => page.goto('/stats'), undefined, true)
    await expect(page.getByRole('button', { name: 'Date and time range', exact: true })).toContainText('Live')
    await expect(page.getByRole('button', { name: 'Previous window', exact: true })).toHaveText('Older')
    await expect(page.getByRole('button', { name: 'Next window', exact: true })).toHaveText('Newer')
    await expect(page.getByRole('button', { name: 'Next window', exact: true })).toBeDisabled()
    const previous = { start: LIVE_END - 48 * HOUR, end: LIVE_END - 24 * HOUR }
    await selectWindow(previous, () => page.getByRole('button', { name: 'Previous window', exact: true }).click())
    const refreshedPrevious = await selectWindow(previous, () => page.clock.fastForward(HOUR))
    await selectWindow({ start: LIVE_END - 72 * HOUR, end: LIVE_END - 48 * HOUR }, () =>
      page.getByRole('button', { name: 'Previous window', exact: true }).click(),
    )
    // The shared QueryClient keeps this just-refreshed key fresh for 10s; paused time
    // means Next legitimately renders that completed generation without another request.
    await selectWindow(
      previous,
      () => page.getByRole('button', { name: 'Next window', exact: true }).click(),
      refreshedPrevious,
    )
    await selectWindow(
      { start: LIVE_END - 23 * HOUR, end: LIVE_END + HOUR },
      () => page.getByRole('button', { name: 'Return to live', exact: true }).click(),
      undefined,
      true,
    )
  })

  test('custom ranges exceed 24 hours and reject unaligned or reversed boundaries without querying', async ({
    page,
  }) => {
    await page.clock.setFixedTime(NOW)
    const requests = await fixture(page)
    await page.goto('/stats')
    await expectWindow(requests, { start: LIVE_END - 24 * HOUR, end: LIVE_END })
    const dialog = await chooseRange(page, '2026-10-01T00:00', '2026-10-04T00:15')
    await dialog.getByRole('button', { name: 'Apply range', exact: true }).click()
    const window = { start: Date.UTC(2026, 9, 1), end: Date.UTC(2026, 9, 4, 0, 15) }
    await expectWindow(requests, window)
    await expect(dialog).not.toBeVisible()
    const invalid = await chooseRange(page, '2026-10-01T00:01', '2026-10-04T00:15')
    const before = requests.length
    await expect(invalid.getByRole('button', { name: 'Apply range', exact: true })).toBeDisabled()
    await expect(invalid.getByRole('alert')).toBeVisible()
    expect(requests).toHaveLength(before)
    await invalid.getByLabel('Start', { exact: true }).fill('2026-10-04T00:15')
    await expect(invalid.getByRole('button', { name: 'Apply range', exact: true })).toBeDisabled()
    await expect(invalid.getByRole('alert')).toBeVisible()
    expect(requests).toHaveLength(before)
    await invalid.getByLabel('Start', { exact: true }).fill('2026-10-05T00:15')
    await expect(invalid.getByRole('button', { name: 'Apply range', exact: true })).toBeDisabled()
    await expect(invalid.getByRole('alert')).toBeVisible()
    expect(requests).toHaveLength(before)
  })

  test('two-year custom window uses effective coarse buckets without truncating coverage', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    const start = Date.UTC(2024, 0, 1)
    const end = Date.UTC(2026, 0, 1)
    // One input and one output token per durable day in the selected interval.
    // Aggregate these fixed facts at every requested grain, including expanded activity ranges.
    const dailyFacts = Array.from({ length: 731 }, (_, index) => start + index * 24 * HOUR)
    const requests = await fixture(page, async (route, url) => {
      const queryStart = Number(url.searchParams.get('start_at'))
      const queryEnd = Number(url.searchParams.get('end_at'))
      if (url.pathname.endsWith('/overview') && queryStart === start && queryEnd === end) {
        await route.fulfill({ json: { data: { ...overview(731), total_input_tokens: 731, total_output_tokens: 731 } } })
        return true
      }
      if (!url.pathname.endsWith('/series')) return false
      const bucketMs = Number(url.searchParams.get('bucket')) * 1000
      const totals = new Map<number, number>()
      for (const fact of dailyFacts) {
        if (fact < queryStart || fact >= queryEnd) continue
        const bucketStart = queryStart + Math.floor((fact - queryStart) / bucketMs) * bucketMs
        totals.set(bucketStart, (totals.get(bucketStart) ?? 0) + 1)
      }
      await route.fulfill({
        json: {
          data: [...totals].map(([bucket_start, tokens]) => ({
            bucket_start,
            bucket_ms: bucketMs,
            request_count: tokens,
            terminal_request_count: tokens,
            failed_request_count: 0,
            upstream_request_count: tokens,
            upstream_error_count: 0,
            total_input_tokens: tokens,
            total_output_tokens: tokens,
            total_cache_read_tokens: 0,
            total_cache_write_tokens: 0,
            total_reasoning_tokens: null,
            avg_duration_ms: 1000,
            avg_first_token_ms: 500,
            avg_output_tps: 1,
          })),
        },
      })
      return true
    })
    await page.goto('/stats')
    const dialog = await chooseRange(page, '2024-01-01T00:00', '2026-01-01T00:00')
    await dialog.getByRole('button', { name: 'Apply range', exact: true }).click()
    await expectWindow(requests, { start, end })
    await expect(page.getByLabel('Latency and speed chart', { exact: true })).toBeVisible()
    const cells = page.getByRole('group', { name: 'Token activity' }).locator('.token-activity-cell')
    await expect(cells.first()).toBeVisible()
    await expect(async () => {
      const plan = await page
        .locator('[data-activity-start]')
        .evaluate((el) => ({
          start: Number(el.getAttribute('data-activity-start')),
          end: Number(el.getAttribute('data-activity-end')),
          bucket: Number(el.getAttribute('data-activity-bucket')),
          overflow: el.scrollWidth > el.clientWidth + 1,
        }))
      expect(plan.start).toBeLessThanOrEqual(start)
      expect(plan.end).toBe(end)
      expect(plan.bucket).toBeGreaterThan(2 * 24 * HOUR)
      expect(plan.bucket % 900_000).toBe(0)
      expect(plan.overflow).toBe(false)
      const count = await cells.count()
      expect(count).toBeLessThanOrEqual(512)
      expect(count * plan.bucket).toBe(end - plan.start)
      const labels = await cells.evaluateAll((els) => els.map((el) => el.getAttribute('aria-label') ?? ''))
      const total = labels.reduce((sum, label) => {
        const match = label.match(/· ([\d,]+) tokens$/)
        expect(match).not.toBeNull()
        return sum + Number(match![1].replaceAll(',', ''))
      }, 0)
      expect(total).toBe(1462)
    }).toPass()
    await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Total requests' })).toContainText('731')
    await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Input Tokens' })).toContainText('731')
  })

  test('deleted and recreated resources with the same name remain distinct and non-actionable', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1280, height: 900 })
    await fixture(page, async (route, url) => {
      const endpoint = url.pathname.split('/').at(-1)
      const identities = [
        { suffix: 'old', deleted: true, requests: 11 },
        { suffix: 'new', deleted: false, requests: 22 },
      ]
      let data: Record<string, unknown>[]
      if (endpoint === 'models') {
        data = identities.map(({ suffix, deleted, requests }) => ({
          model_id: `model-${suffix}`,
          model: 'Reused model name',
          deleted,
          request_count: requests,
          total_input_tokens: requests,
          total_output_tokens: requests,
          total_reasoning_tokens: null,
          avg_duration_ms: 1000,
        }))
      } else if (endpoint === 'providers') {
        data = identities.map(({ suffix, deleted, requests }) => ({
          provider_id: `provider-${suffix}`,
          provider: 'Reused service name',
          deleted,
          upstream_request_count: requests,
          upstream_error_count: 0,
          avg_output_tps: 1,
        }))
      } else if (endpoint === 'api-keys') {
        data = identities.map(({ suffix, deleted, requests }) => ({
          api_key_id: `key-${suffix}`,
          api_key_name: 'Reused key name',
          deleted,
          request_count: requests,
          total_input_tokens: requests,
          total_output_tokens: requests,
          cache_read_tokens: 0,
          cache_write_tokens: 0,
          reasoning_tokens: null,
          last_used_at: NOW,
        }))
      } else return false
      await route.fulfill({ json: { data } })
      return true
    })
    await page.goto('/stats')
    const models = page.getByRole('region', { name: 'Models', exact: true })
    const modelItems = models.getByRole('listitem')
    await expect(modelItems).toHaveCount(2)
    const deletedModel = modelItems.filter({ hasText: /Reused model name.*Deleted/i })
    const liveModel = modelItems.filter({ hasText: 'Reused model name' }).filter({ hasNotText: /Deleted/i })
    await expect(deletedModel).toHaveCount(1)
    await expect(liveModel).toHaveCount(1)
    await expect(deletedModel).toContainText('33.33%')
    await expect(liveModel).toContainText('66.67%')
    await expect(deletedModel.getByRole('link')).toHaveCount(0)
    await expect(deletedModel.getByRole('button')).toHaveCount(0)

    for (const [tableName, resourceName] of [
      ['Providers', 'Reused service name'],
      ['API Key usage', 'Reused key name'],
    ] as const) {
      const table = page.getByRole('table', { name: tableName, exact: true })
      const rows = table.getByRole('row').filter({ hasText: resourceName })
      await expect(rows).toHaveCount(2)
      const deletedRow = rows.filter({ hasText: /Deleted/i })
      const liveRow = rows.filter({ hasNotText: /Deleted/i })
      await expect(deletedRow).toHaveCount(1)
      await expect(liveRow).toHaveCount(1)
      await expect(deletedRow.getByRole('cell', { name: '11', exact: true }).first()).toBeVisible()
      await expect(liveRow.getByRole('cell', { name: '22', exact: true }).first()).toBeVisible()
      await expect(deletedRow.getByRole('link')).toHaveCount(0)
      await expect(deletedRow.getByRole('button')).toHaveCount(0)
    }
  })

  test('delayed old-window responses cannot overwrite a newer preset', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    let release!: () => void
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    let signalHeld!: () => void
    const started = new Promise<void>((resolve) => {
      signalHeld = resolve
    })
    const requests = await fixture(page, async (route, url) => {
      const span = Number(url.searchParams.get('end_at')) - Number(url.searchParams.get('start_at'))
      if (!url.pathname.endsWith('/overview') || span !== 6 * HOUR) return false
      signalHeld()
      await held
      await route.fulfill({ json: { data: overview(999) } })
      return true
    })
    await page.goto('/stats')
    await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Total requests' })).toContainText('12')
    await page.locator('[data-slot="select-trigger"]').click()
    await page.getByRole('option', { name: 'Last 6h', exact: true }).click()
    await started
    await page.locator('[data-slot="select-trigger"]').click()
    await page.getByRole('option', { name: 'Last 3d', exact: true }).click()
    await expectWindow(requests, { start: LIVE_END - 72 * HOUR, end: LIVE_END })
    const response = page.waitForResponse(
      (r) =>
        r.url().includes('/stats/overview') &&
        Number(new URL(r.url()).searchParams.get('end_at')) - Number(new URL(r.url()).searchParams.get('start_at')) ===
          6 * HOUR,
    )
    release()
    await response
    await expect(page.locator('[data-slot="select-trigger"]')).toContainText('Last 3d')
    await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Total requests' })).toContainText('12')
    await expect(page.getByText('999', { exact: true })).toHaveCount(0)
  })
})

test.describe('statistics local time across DST', () => {
  test.use({ timezoneId: 'America/New_York' })

  test('cross-midnight spring-forward selection sends the exact absolute interval', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    const requests = await fixture(page)
    await page.goto('/stats')
    const dialog = await chooseRange(page, '2026-03-07T23:45', '2026-03-09T00:00')
    await dialog.getByRole('button', { name: 'Apply range', exact: true }).click()
    await expectWindow(requests, { start: Date.UTC(2026, 2, 8, 4, 45), end: Date.UTC(2026, 2, 9, 4) })
    await page.getByRole('button', { name: 'Date and time range', exact: true }).click()
    await expect(page.getByRole('dialog').getByLabel('Start', { exact: true })).toHaveValue('2026-03-07T23:45')
    await expect(page.getByRole('dialog').getByLabel('End', { exact: true })).toHaveValue('2026-03-09T00:00')
  })

  for (const [name, start, end] of [
    ['nonexistent spring-forward time', '2026-03-08T02:15', '2026-03-08T04:00'],
    ['ambiguous fall-back time', '2026-11-01T01:15', '2026-11-01T03:00'],
  ] as const) {
    test(`rejects ${name} rather than silently changing the interval`, async ({ page }) => {
      await page.clock.setFixedTime(Date.UTC(2026, 10, 2, 12, 7))
      const requests = await fixture(page)
      await page.goto('/stats')
      await expect.poll(() => requests.length).toBeGreaterThanOrEqual(5)
      const dialog = await chooseRange(page, start, end)
      const before = requests.length
      await expect(dialog.getByRole('button', { name: 'Apply range', exact: true })).toBeDisabled()
      await expect(dialog.getByRole('alert')).toBeVisible()
      expect(requests).toHaveLength(before)
    })
  }
})

test.describe('statistics range dialog validation', () => {
  test.use({ timezoneId: 'UTC' })

  test('states the quarter-hour rule once and reports each problem next to its field', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    const requests = await fixture(page)
    await page.goto('/stats')
    await expectWindow(requests, { start: LIVE_END - 24 * HOUR, end: LIVE_END })
    await page.getByRole('button', { name: 'Date and time range', exact: true }).click()
    const dialog = page.getByRole('dialog')
    await expect(dialog.getByText(/quarter-hour/)).toHaveCount(1)
    await expect(dialog.getByRole('alert')).toHaveCount(0)

    const start = dialog.getByLabel('Start', { exact: true })
    const end = dialog.getByLabel('End', { exact: true })
    const apply = dialog.getByRole('button', { name: 'Apply range', exact: true })
    const before = requests.length

    await start.fill('2026-10-01T00:01')
    const startError = dialog.locator('[data-slot="field"]:has(#stats-start)').getByRole('alert')
    const endError = dialog.locator('[data-slot="field"]:has(#stats-end)').getByRole('alert')
    await expect(startError).toBeVisible()
    await expect(endError).toHaveCount(0)
    await expect(start).toHaveAttribute('aria-invalid', 'true')
    await expect(apply).toBeDisabled()

    await start.fill('2026-10-04T00:15')
    await end.fill('2026-10-04T00:15')
    await expect(endError).toContainText('later than the start')
    await expect(startError).toHaveCount(0)

    await start.fill('2026-10-10T00:00')
    await end.fill('2026-10-12T00:00')
    await expect(endError).toContainText('current 15-minute interval')
    await expect(apply).toBeDisabled()
    expect(requests).toHaveLength(before)

    await start.fill('2026-10-01T00:00')
    await end.fill('2026-10-04T00:15')
    await expect(dialog.getByRole('alert')).toHaveCount(0)
    await expect(dialog.getByText(/quarter-hour/)).toHaveCount(1)
    await expect(apply).toBeEnabled()
  })
})

const resourceTotal = 12
const providerRows = Array.from({ length: resourceTotal }, (_, index) => ({
  provider_id: `provider-${index + 1}`,
  provider: `Service ${String(index + 1).padStart(2, '0')}`,
  deleted: false,
  // Backend order is by volume, so the smallest rows sit outside the default top eight.
  upstream_request_count: (resourceTotal - index) * 100,
  upstream_error_count: index === 0 ? 3 : 0,
  avg_duration_ms: 100,
  avg_output_tps: 10 + index,
}))
const keyRows = Array.from({ length: resourceTotal }, (_, index) => ({
  api_key_id: `key-${index + 1}`,
  api_key_name: `Client ${String(index + 1).padStart(2, '0')}`,
  deleted: false,
  request_count: (resourceTotal - index) * 100,
  total_input_tokens: 1_000 + index,
  total_output_tokens: 200 + index,
  cache_read_tokens: 50,
  cache_write_tokens: 5,
  reasoning_tokens: null,
  last_used_at: NOW - index * HOUR,
}))

async function resourceFixture(page: Page, intercept?: (route: Route, url: URL) => Promise<boolean>) {
  return fixture(page, async (route, url) => {
    if (intercept && (await intercept(route, url))) return true
    const endpoint = url.pathname.split('/').at(-1)
    if (endpoint === 'providers') await route.fulfill({ json: { data: providerRows } })
    else if (endpoint === 'api-keys') await route.fulfill({ json: { data: keyRows } })
    else return false
    return true
  })
}

test.describe('statistics resource breakdowns', () => {
  test.use({ timezoneId: 'UTC' })

  for (const [name, regionName, label, firstOutside] of [
    ['providers', 'Providers', 'Service', 'Service 09'],
    ['API Keys', 'API Key usage', 'Client', 'Client 09'],
  ] as const) {
    test(`more than eight ${name} are labelled as the top eight and can be viewed in full`, async ({ page }) => {
      await page.clock.setFixedTime(NOW)
      await page.setViewportSize({ width: 1600, height: 1000 })
      await resourceFixture(page)
      await page.goto('/stats')
      const region = page.getByRole('region', { name: regionName, exact: true })
      const rows = region.getByRole('table').getByRole('row')
      await expect(rows).toHaveCount(1 + 8)
      await expect(region.getByText('Top 8 of 12', { exact: true })).toBeVisible()
      await expect(region.getByRole('cell', { name: `${label} 08`, exact: true })).toBeVisible()
      await expect(region.getByRole('cell', { name: firstOutside, exact: true })).toHaveCount(0)

      await region.getByRole('button', { name: 'View all 12', exact: true }).click()
      await expect(rows).toHaveCount(1 + 12)
      await expect(region.getByRole('cell', { name: `${label} 12`, exact: true })).toBeVisible()
      await expect(region.getByText('Top 8 of 12')).toHaveCount(0)

      await region.getByRole('button', { name: 'Show top 8', exact: true }).click()
      await expect(rows).toHaveCount(1 + 8)
      await expect(region.getByRole('button', { name: 'View all 12', exact: true })).toBeVisible()
    })

    test(`sorting ${name} covers the whole collection before the top eight are taken`, async ({ page }) => {
      await page.clock.setFixedTime(NOW)
      await page.setViewportSize({ width: 1600, height: 1000 })
      await resourceFixture(page)
      await page.goto('/stats')
      const region = page.getByRole('region', { name: regionName, exact: true })
      const table = region.getByRole('table')
      await expect(table.getByRole('row').nth(1)).toContainText(`${label} 01`)

      const header = table.getByRole('columnheader', { name: /Requests/ })
      await header.getByRole('button').click()
      if ((await header.getAttribute('aria-sort')) !== 'ascending') await header.getByRole('button').click()
      await expect(header).toHaveAttribute('aria-sort', 'ascending')
      // The smallest row was ninth or later in backend order; it leads once the full set is sorted.
      await expect(table.getByRole('row').nth(1)).toContainText(`${label} 12`)
      await expect(table.getByRole('row')).toHaveCount(1 + 8)
      await expect(region.getByRole('cell', { name: `${label} 01`, exact: true })).toHaveCount(0)

      await region.getByRole('button', { name: 'View all 12', exact: true }).click()
      await expect(table.getByRole('row')).toHaveCount(1 + 12)
      await expect(table.getByRole('row').nth(1)).toContainText(`${label} 12`)
      await expect(table.getByRole('row').nth(12)).toContainText(`${label} 01`)
    })

    test(`${name} stay complete and reachable in the narrow list`, async ({ page }) => {
      await page.clock.setFixedTime(NOW)
      await page.setViewportSize({ width: 320, height: 800 })
      await resourceFixture(page)
      await page.goto('/stats')
      const region = page.getByRole('region', { name: regionName, exact: true })
      const list = region.locator('.route-mobile-row')
      await expect(list).toHaveCount(8)
      await expect(region.getByText('Top 8 of 12', { exact: true })).toBeVisible()
      const viewAll = region.getByRole('button', { name: 'View all 12', exact: true })
      await viewAll.scrollIntoViewIfNeeded()
      const box = (await viewAll.boundingBox())!
      expect(box.x).toBeGreaterThanOrEqual(0)
      expect(box.x + box.width).toBeLessThanOrEqual(320)
      await viewAll.click()
      await expect(list).toHaveCount(12)
      await expect(list.last()).toContainText(`${label} 12`)
      expect(
        await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1),
      ).toBe(true)
    })
  }

  test('only real upstream errors are destructive and zero upstream requests stay unknown', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await resourceFixture(page, async (route, url) => {
      if (url.pathname.split('/').at(-1) !== 'providers') return false
      await route.fulfill({
        json: {
          data: [
            {
              provider_id: 'p-failing',
              provider: 'Service 01',
              deleted: false,
              upstream_request_count: 100,
              upstream_error_count: 3,
              avg_duration_ms: 100,
              avg_output_tps: 10,
            },
            {
              provider_id: 'p-healthy',
              provider: 'Service 02',
              deleted: false,
              upstream_request_count: 100,
              upstream_error_count: 0,
              avg_duration_ms: 100,
              avg_output_tps: 11,
            },
            {
              provider_id: 'p-idle',
              provider: 'Service 03',
              deleted: false,
              upstream_request_count: 0,
              upstream_error_count: 0,
              avg_duration_ms: null,
              avg_output_tps: null,
            },
          ],
        },
      })
      return true
    })
    await page.goto('/stats')
    const providers = page.getByRole('region', { name: 'Providers', exact: true })
    const colorOf = (locator: Locator) => locator.evaluate((el) => getComputedStyle(el).color)
    for (const header of ['Upstream Requests', 'Upstream Errors', 'Upstream Error Rate']) {
      // 列头必须字面写出“上游”，排序按钮的可访问名称以它开头。
      await expect(providers.getByRole('columnheader', { name: new RegExp(`^Sort ${header} `) })).toBeVisible()
    }
    const failing = providers.getByRole('row').filter({ hasText: 'Service 01' })
    const healthy = providers.getByRole('row').filter({ hasText: 'Service 02' })
    const idle = providers.getByRole('row').filter({ hasText: 'Service 03' })
    const errorsColumn = 2
    const rateColumn = 3
    const failingColor = await colorOf(failing.getByRole('cell').nth(errorsColumn))
    const healthyColor = await colorOf(healthy.getByRole('cell').nth(errorsColumn))
    await expect(failing.getByRole('cell').nth(errorsColumn)).toHaveText('3')
    await expect(healthy.getByRole('cell').nth(errorsColumn)).toHaveText('0')
    expect(healthyColor).toBe(await colorOf(healthy.getByRole('cell').nth(1)))
    expect(failingColor).not.toBe(healthyColor)
    await expect(failing.getByRole('cell').nth(rateColumn)).toHaveText('3%')
    await expect(healthy.getByRole('cell').nth(rateColumn)).toHaveText('0%')
    // 没有上游请求时错误率未知，保持中性的“—”，不能读成 0% 的健康。
    await expect(idle.getByRole('cell').nth(rateColumn)).toHaveText('—')
    expect(await colorOf(idle.getByRole('cell').nth(rateColumn))).toBe(await colorOf(healthy.getByRole('cell').nth(1)))

    await page.setViewportSize({ width: 390, height: 900 })
    const failingList = providers.locator('.route-mobile-row').filter({ hasText: 'Service 01' })
    const healthyList = providers.locator('.route-mobile-row').filter({ hasText: 'Service 02' })
    const idleList = providers.locator('.route-mobile-row').filter({ hasText: 'Service 03' })
    const failingError = failingList.getByText('3 upstream errors', { exact: true })
    const healthyError = healthyList.getByText('0 upstream errors', { exact: true })
    expect(await colorOf(failingError)).toBe(failingColor)
    expect(await colorOf(healthyError)).not.toBe(failingColor)
    await expect(failingList).toContainText('3%')
    await expect(healthyList).toContainText('0%')
    await expect(idleList).toContainText('—')
    await expect(idleList.getByText('0', { exact: true })).toBeVisible()
  })
})

test.describe('statistics report without traffic', () => {
  test.use({ timezoneId: 'UTC' })

  const emptyOverview = {
    total_requests: 0,
    terminal_request_count: 0,
    failed_request_count: 0,
    upstream_request_count: 0,
    upstream_error_count: 0,
    total_input_tokens: 0,
    total_output_tokens: 0,
    total_cache_read_tokens: 0,
    total_cache_write_tokens: 0,
    total_reasoning_tokens: null,
    avg_duration_ms: null,
    avg_first_token_ms: null,
    avg_output_tps: null,
  }
  const emptyReport = async (route: Route, url: URL) => {
    const endpoint = url.pathname.split('/').at(-1)
    await route.fulfill({ json: { data: endpoint === 'overview' ? emptyOverview : [] } })
    return true
  }

  test('says there is no traffic once while the activity grid stays a zero grid', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await fixture(page, emptyReport)
    await page.goto('/stats')
    await expect(page.getByText('No usage recorded in this time range.', { exact: true })).toHaveCount(1)

    const activity = page.getByRole('group', { name: 'Token activity' })
    const cells = activity.locator('.token-activity-cell')
    await expect(cells.first()).toBeVisible()
    expect(await cells.count()).toBeGreaterThan(8)
    expect(await cells.evaluateAll((els) => els.every((el) => el.getAttribute('data-level') === '0'))).toBe(true)
    expect(
      await cells.evaluateAll((els) => els.every((el) => /· 0 tokens$/.test(el.getAttribute('aria-label') ?? ''))),
    ).toBe(true)

    // Every other section keeps only a short state line: no repeated bordered empty cards.
    for (const [region, text] of [
      ['Latency and speed', 'No latency or speed data.'],
      ['Availability', 'No availability data.'],
      ['Token breakdown', 'No token data.'],
      ['Models', 'No model traffic.'],
      ['Providers', 'No provider traffic.'],
      ['API Key usage', 'No API Key traffic.'],
    ] as const) {
      const section = page.getByRole('region', { name: region, exact: true })
      await expect(section.getByText(text, { exact: true })).toBeVisible()
      await expect(section.locator('[data-slot="empty"]')).toHaveCount(0)
      // Latency shares its grid rows with the always-present zero activity grid.
      if (region !== 'Latency and speed') expect((await section.boundingBox())!.height).toBeLessThan(200)
    }
    await expect(page.getByRole('alert')).toHaveCount(0)
  })

  test('a failed section is reported as failed, not merged into the no-traffic state', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await fixture(page, async (route, url) => {
      if (url.pathname.endsWith('/models')) {
        await route.fulfill({ status: 500, json: { error: 'Model statistics unavailable' } })
        return true
      }
      return emptyReport(route, url)
    })
    await page.goto('/stats')
    const models = page.getByRole('region', { name: 'Models', exact: true })
    await expect(models.getByRole('alert')).toContainText('Model statistics unavailable')
    await expect(models.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
    await expect(models.getByText('No model traffic.')).toHaveCount(0)
    await expect(page.getByText('Some usage data could not be refreshed')).toBeVisible()
    // The successful empty summary and the other sections remain honest about what they know.
    await expect(page.getByText('No usage recorded in this time range.', { exact: true })).toHaveCount(1)
    await expect(
      page.getByRole('region', { name: 'Providers', exact: true }).getByText('No provider traffic.'),
    ).toBeVisible()
  })

  test('a failed summary is not presented as an empty report', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await fixture(page, async (route, url) => {
      if (url.pathname.endsWith('/overview')) {
        await route.fulfill({ status: 500, json: { error: 'Summary unavailable' } })
        return true
      }
      return emptyReport(route, url)
    })
    await page.goto('/stats')
    await expect(page.getByText('Usage summary unavailable')).toBeVisible()
    await expect(page.getByText('No usage recorded in this time range.')).toHaveCount(0)
  })
})
