import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

const QUARTER = 900_000
const HOUR = 3_600_000
const END = Date.UTC(2026, 8, 17, 8, 15)
const PEAK = END - 9 * HOUR
const UNKNOWN = END - 2 * QUARTER
const MISSING = END - 3 * QUARTER

test.use({ timezoneId: 'UTC' })

async function stubStats(page: Page) {
  await page.clock.setFixedTime(END - 8 * 60_000)
  const requests: URL[] = []
  await page.route('**/api/v1/stats/**', async (route) => {
    const url = new URL(route.request().url())
    requests.push(url)
    const start = Number(url.searchParams.get('start_at'))
    const end = Number(url.searchParams.get('end_at'))
    if (url.pathname.endsWith('/overview')) {
      await route.fulfill({
        json: {
          data: {
            total_requests: 128,
            error_count: 2,
            total_input_tokens: 600,
            total_output_tokens: 86,
            total_cache_read_tokens: 320,
            total_cache_write_tokens: 12,
            total_reasoning_tokens: null,
            avg_duration_ms: 120,
            avg_first_token_ms: 40,
            avg_output_tps: 25,
          },
        },
      })
    } else if (url.pathname.endsWith('/series')) {
      const bucket = Number(url.searchParams.get('bucket')) * 1000
      const data = []
      // Aggregate durable quarter-hour facts at the requested grain, aligned to request start.
      for (let at = start; at < end; at += bucket) {
        let tokens = 0
        let known = false
        let traffic = false
        for (let quarter = at; quarter < Math.min(at + bucket, end); quarter += QUARTER) {
          if (quarter === MISSING) continue
          traffic = true
          if (quarter === UNKNOWN) continue
          known = true
          tokens += quarter === PEAK ? 1_000_000 : 100
        }
        if (!traffic) continue
        data.push({
          bucket_start: at,
          bucket_ms: bucket,
          request_count: 1,
          error_count: 0,
          total_input_tokens: known ? tokens : null,
          total_output_tokens: known ? 0 : null,
          total_cache_read_tokens: 80,
          total_cache_write_tokens: 10,
          total_reasoning_tokens: null,
          avg_duration_ms: 120,
          avg_first_token_ms: 40,
          avg_output_tps: 25,
        })
      }
      await route.fulfill({ json: { data } })
    } else {
      await route.fulfill({ json: { data: [] } })
    }
  })
  return requests
}

async function expectCoverage(page: Page, selectedStart: number, end: number, requests: URL[]) {
  const wrapper = page.locator('[data-activity-start]')
  const cells = page.getByRole('group', { name: 'Token activity' }).locator('.token-activity-cell')
  await expect(cells.first()).toBeVisible()
  await expect(async () => {
    const plan = await wrapper.evaluate((el) => ({
      start: Number(el.getAttribute('data-activity-start')),
      end: Number(el.getAttribute('data-activity-end')),
      bucket: Number(el.getAttribute('data-activity-bucket')),
      overflows: el.scrollWidth > el.clientWidth + 1,
    }))
    expect(plan.start).toBeLessThanOrEqual(selectedStart)
    expect(plan.end).toBe(end)
    expect(plan.bucket % QUARTER).toBe(0)
    expect(plan.overflows).toBe(false)
    const starts = await cells.evaluateAll((els) => els.map((el) => Number(el.getAttribute('data-start'))))
    expect(starts.length).toBeLessThanOrEqual(512)
    expect(starts).toEqual(
      Array.from({ length: (end - plan.start) / plan.bucket }, (_, i) => plan.start + i * plan.bucket),
    )
    expect(starts.at(-1)! + plan.bucket).toBe(end)
    expect(
      requests.some(
        (url) =>
          url.pathname.endsWith('/series') &&
          Number(url.searchParams.get('start_at')) === plan.start &&
          Number(url.searchParams.get('end_at')) === end &&
          Number(url.searchParams.get('bucket')) * 1000 === plan.bucket,
      ),
    ).toBe(true)
  }).toPass()
  return { wrapper, cells }
}

test('presets retain base grain while real history fills the available width', async ({ page }) => {
  await prepareApp(page)
  const requests = await stubStats(page)
  await page.setViewportSize({ width: 1600, height: 900 })
  await page.goto('/stats')
  for (const [label, hours, bucket] of [
    ['Last 6h', 6, QUARTER],
    ['Last 24h', 24, HOUR],
    ['Last 3d', 72, 6 * HOUR],
    ['Last 7d', 168, 24 * HOUR],
  ] as const) {
    await page.locator('[data-slot="select-trigger"]').click()
    await page.getByRole('option', { name: label, exact: true }).click()
    const { wrapper } = await expectCoverage(page, END - hours * HOUR, END, requests)
    await expect(wrapper).toHaveAttribute('data-activity-bucket', String(bucket))
    const latency = page.getByLabel('Latency and speed chart', { exact: true })
    await expect(latency).toBeVisible()
    await expect(async () => {
      const activityBox = (await wrapper.boundingBox())!
      const latencyBox = (await latency.boundingBox())!
      expect(Math.abs(activityBox.y - latencyBox.y)).toBeLessThanOrEqual(1)
      expect(Math.abs(activityBox.y + activityBox.height - latencyBox.y - latencyBox.height)).toBeLessThanOrEqual(1)
      expect(await wrapper.evaluate((el) => el.scrollHeight > el.clientHeight)).toBe(false)
    }).toPass()
  }
})

test('historical peak sets the whole grid scale, missing buckets are zero and null stays unknown', async ({ page }) => {
  await prepareApp(page)
  const requests = await stubStats(page)
  await page.setViewportSize({ width: 1600, height: 900 })
  await page.goto('/stats')
  await page.locator('[data-slot="select-trigger"]').click()
  await page.getByRole('option', { name: 'Last 6h', exact: true }).click()
  const selectedStart = END - 6 * HOUR
  const { wrapper } = await expectCoverage(page, selectedStart, END, requests)
  expect(Number(await wrapper.getAttribute('data-activity-start'))).toBeLessThan(PEAK)
  const peak = page.locator(`.token-activity-cell[data-start="${PEAK}"]`)
  await expect(peak).toHaveAttribute('aria-label', /1M tokens$/)
  await expect(peak).toHaveAttribute('data-level', '4')
  const newest = page.locator(`.token-activity-cell[data-start="${END - QUARTER}"]`)
  await expect(newest).toHaveAttribute('aria-label', /100 tokens$/)
  await expect(newest).toHaveAttribute('data-level', '1')
  await expect(page.locator(`.token-activity-cell[data-start="${MISSING}"]`)).toHaveAttribute('aria-label', /0 tokens$/)
  await expect(page.locator(`.token-activity-cell[data-start="${UNKNOWN}"]`)).toHaveAttribute(
    'aria-label',
    /token usage unknown$/,
  )
  for (const endpoint of ['overview', 'models', 'providers', 'api-keys']) {
    expect(
      requests.some(
        (url) =>
          url.pathname.endsWith(`/${endpoint}`) &&
          Number(url.searchParams.get('start_at')) === selectedStart &&
          Number(url.searchParams.get('end_at')) === END,
      ),
    ).toBe(true)
  }
  expect(
    requests.some(
      (url) =>
        url.pathname.endsWith('/series') &&
        url.searchParams.get('bucket') === '3600' &&
        Number(url.searchParams.get('start_at')) === selectedStart &&
        Number(url.searchParams.get('end_at')) === END,
    ),
  ).toBe(true)
  const openTooltip = page.locator('[data-slot="tooltip-content"][data-state$="open"]')
  await newest.hover()
  await expect(openTooltip).toContainText('100 tokens')
  await expect(openTooltip).toContainText('08:00')
  await page.locator(`.token-activity-cell[data-start="${UNKNOWN}"]`).hover()
  await expect(openTooltip).toContainText('token usage unknown')
})

test('widening the viewport queries and renders more earlier history', async ({ page }) => {
  await prepareApp(page)
  const requests = await stubStats(page)
  // Both widths use the 12-column composition (workspace >= 80rem), so the activity column grows with the viewport.
  await page.setViewportSize({ width: 1600, height: 900 })
  await page.goto('/stats')
  const { wrapper, cells } = await expectCoverage(page, END - 24 * HOUR, END, requests)
  const before = Number(await wrapper.getAttribute('data-activity-start'))
  const count = await cells.count()
  await page.setViewportSize({ width: 2560, height: 900 })
  await expect.poll(async () => Number(await wrapper.getAttribute('data-activity-start'))).toBeLessThan(before)
  await expectCoverage(page, END - 24 * HOUR, END, requests)
  expect(await cells.count()).toBeGreaterThan(count)
})

test('narrow screens coarsen a long custom range without truncating or horizontal scrolling', async ({ page }) => {
  await prepareApp(page)
  const requests = await stubStats(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/stats')
  await page.getByRole('button', { name: 'Date and time range', exact: true }).click()
  const dialog = page.getByRole('dialog')
  await dialog.getByLabel('Start', { exact: true }).fill('2024-01-01T00:00')
  await dialog.getByLabel('End', { exact: true }).fill('2026-01-01T00:00')
  await dialog.getByRole('button', { name: 'Apply range', exact: true }).click()
  const { wrapper } = await expectCoverage(page, Date.UTC(2024, 0, 1), Date.UTC(2026, 0, 1), requests)
  expect(Number(await wrapper.getAttribute('data-activity-bucket'))).toBeGreaterThan(2 * 24 * HOUR)
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth + 1),
  ).toBe(false)
})

test('token breakdown keeps net input without adding cache subsets back', async ({ page }) => {
  await prepareApp(page)
  await stubStats(page)
  await page.route('**/api/v1/stats/overview**', async (route) => {
    await route.fulfill({
      json: {
        data: {
          total_requests: 1,
          total_input_tokens: 20,
          total_output_tokens: 20,
          total_cache_read_tokens: 80,
          total_cache_write_tokens: 10,
          total_reasoning_tokens: null,
          avg_duration_ms: 1000,
          avg_first_token_ms: 500,
          avg_output_tps: 20,
          error_count: 0,
        },
      },
    })
  })
  await page.goto('/stats')
  const breakdown = page.getByRole('region', { name: 'Token breakdown', exact: true })
  await expect(breakdown).toBeVisible()
  await expect(breakdown.getByText('40', { exact: true })).toBeVisible()
  await expect(breakdown.getByRole('listitem')).toHaveCount(2)
})

const rem = 16

async function populateResources(page: Page, total = 12) {
  await page.route('**/api/v1/stats/providers**', async (route) => {
    await route.fulfill({
      json: {
        data: Array.from({ length: total }, (_, index) => ({
          provider_id: `provider-${index + 1}`,
          provider: `Service ${String(index + 1).padStart(2, '0')}`,
          deleted: false,
          request_count: (total - index) * 100,
          error_count: index === 0 ? 3 : 0,
          avg_duration_ms: 100,
          avg_output_tps: 10 + index,
        })),
      },
    })
  })
  await page.route('**/api/v1/stats/api-keys**', async (route) => {
    await route.fulfill({
      json: {
        data: Array.from({ length: total }, (_, index) => ({
          api_key_id: `key-${index + 1}`,
          api_key_name: `Client ${String(index + 1).padStart(2, '0')}`,
          deleted: false,
          request_count: (total - index) * 100,
          total_input_tokens: 1_234_567 + index,
          total_output_tokens: 234_567 + index,
          cache_read_tokens: 54_321,
          cache_write_tokens: 4_321,
          reasoning_tokens: null,
          last_used_at: END - index * HOUR,
        })),
      },
    })
  })
  await page.route('**/api/v1/stats/models**', async (route) => {
    await route.fulfill({
      json: {
        data: ['gpt-5', 'claude-opus', 'gemini-pro', 'deepseek', 'qwen'].map((model, index) => ({
          model_id: `model-${index}`,
          deleted: false,
          model,
          request_count: 50 - index * 7,
          total_input_tokens: 100,
          total_output_tokens: 10,
          total_reasoning_tokens: null,
          avg_duration_ms: 100,
        })),
      },
    })
  })
}

async function usageBoxes(page: Page) {
  const region = (name: string) => page.getByRole('region', { name, exact: true })
  const box = async (name: string) => (await region(name).boundingBox())!
  return {
    region,
    page: (await page.locator('.route-page').boundingBox())!,
    activity: await box('Token activity'),
    latency: await box('Latency and speed'),
    errors: await box('Errors'),
    breakdown: await box('Token breakdown'),
    models: await box('Models'),
    providers: await box('Providers'),
    keys: await box('API Key usage'),
  }
}

test.describe('usage workspace layout', () => {
  test('2560 spends the added width on history, plots and tables while donuts stay bounded', async ({ page }) => {
    await prepareApp(page)
    await stubStats(page)
    await populateResources(page)
    await page.setViewportSize({ width: 2560, height: 1440 })
    await page.goto('/stats')
    const wrapper = page.locator('[data-activity-start]')
    await expect(wrapper.locator('.token-activity-cell').first()).toBeVisible()
    const latencyChart = page.getByLabel('Latency and speed chart', { exact: true })
    await expect(latencyChart).toBeVisible()
    const keyTable = page.getByRole('region', { name: 'API Key usage', exact: true }).getByRole('table')
    await expect(keyTable).toBeVisible()
    const boxes = await usageBoxes(page)
    expect(Math.round(boxes.page.width)).toBe(120 * rem)

    // Activity history and the latency plot are the widest analyses, with unchanged 10rem plots.
    const activityBox = (await wrapper.boundingBox())!
    const latencyBox = (await latencyChart.boundingBox())!
    expect(activityBox.width).toBeGreaterThan(60 * rem)
    expect(latencyBox.width).toBeGreaterThan(30 * rem)
    expect(Math.abs(activityBox.height - 10 * rem)).toBeLessThanOrEqual(1)
    expect(Math.abs(latencyBox.height - 10 * rem)).toBeLessThanOrEqual(1)
    expect(Math.abs(activityBox.y - latencyBox.y)).toBeLessThanOrEqual(1)
    expect(Math.abs(activityBox.y + activityBox.height - latencyBox.y - latencyBox.height)).toBeLessThanOrEqual(1)
    const cellSize = await wrapper
      .locator('.token-activity-cell')
      .first()
      .evaluate((el) => el.getBoundingClientRect().width)
    expect(cellSize).toBeLessThanOrEqual(16)

    // Breakdown donuts stay 10-12rem; the error trend takes the width instead.
    for (const [name, label] of [
      ['Token breakdown', 'Token breakdown'],
      ['Models', 'Share of all requests.'],
    ] as const) {
      const donut = boxes.region(name).locator(`div[aria-label="${label}"]`)
      const donutBox = (await donut.boundingBox())!
      expect(donutBox.width).toBeGreaterThanOrEqual(10 * rem - 1)
      expect(donutBox.width).toBeLessThanOrEqual(12 * rem + 1)
      expect(Math.abs(donutBox.width - donutBox.height)).toBeLessThanOrEqual(1)
    }
    expect(boxes.errors.width).toBeGreaterThan(boxes.breakdown.width * 2)
    expect(boxes.breakdown.width).toBeLessThanOrEqual(24 * rem)
    expect(boxes.models.width).toBeLessThanOrEqual(24 * rem)

    // Provider and key tables sit side by side only because the 7-column key table stays readable.
    expect(Math.abs(boxes.providers.y - boxes.keys.y)).toBeLessThanOrEqual(1)
    expect(boxes.keys.x).toBeGreaterThan(boxes.providers.x + boxes.providers.width - 1)
    expect(boxes.keys.width).toBeGreaterThanOrEqual(46 * rem)
    await expect(async () => {
      const headers = await keyTable
        .getByRole('columnheader')
        .evaluateAll((els) =>
          els.map((el) => ({ text: el.textContent?.trim(), clipped: el.scrollWidth > el.clientWidth + 1 })),
        )
      expect(headers).toHaveLength(7)
      expect(headers.filter((header) => header.clipped)).toEqual([])
      const scrolls = await keyTable.evaluate((table) => {
        for (let el = table.parentElement; el; el = el.parentElement) {
          if (el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).overflowX !== 'visible') return true
          if (el.matches('section')) break
        }
        return false
      })
      expect(scrolls).toBe(false)
    }).toPass()
    // The identity column absorbs the remaining width.
    const nameColumn = (await keyTable.getByRole('columnheader').first().boundingBox())!
    const requestColumn = (await keyTable.getByRole('columnheader').nth(1).boundingBox())!
    expect(nameColumn.width).toBeGreaterThan(requestColumn.width * 1.5)
  })

  test('1440 stacks the provider and key tables instead of squeezing the 7-column table', async ({ page }) => {
    await prepareApp(page)
    await stubStats(page)
    await populateResources(page)
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/stats')
    const keyTable = page.getByRole('region', { name: 'API Key usage', exact: true }).getByRole('table')
    await expect(keyTable).toBeVisible()
    const boxes = await usageBoxes(page)
    expect(boxes.keys.y).toBeGreaterThanOrEqual(boxes.providers.y + boxes.providers.height - 1)
    expect(boxes.keys.width).toBeCloseTo(boxes.page.width, 0)
    // Analyses stack in reading order below the 80rem workspace.
    expect(boxes.latency.y).toBeGreaterThanOrEqual(boxes.activity.y + boxes.activity.height - 1)
  })

  test('the 12-column composition follows workspace width at 80rem and the sidebar toggle, not the viewport', async ({
    page,
  }) => {
    await prepareApp(page)
    await stubStats(page)
    await populateResources(page)
    await page.setViewportSize({ width: 1600, height: 900 })
    await page.goto('/stats')
    const sideBySide = async () => {
      const boxes = await usageBoxes(page)
      return {
        width: boxes.page.width,
        paired: Math.abs(boxes.activity.y - boxes.latency.y) <= 1 && boxes.latency.x > boxes.activity.x,
        breakdownRow:
          Math.abs(boxes.errors.y - boxes.breakdown.y) <= 1 && Math.abs(boxes.breakdown.y - boxes.models.y) <= 1,
      }
    }
    await expect(page.locator('[data-activity-start]').locator('.token-activity-cell').first()).toBeVisible()
    const reference = (await page.locator('.route-page').boundingBox())!.width
    const chrome = 1600 - reference

    for (const [workspace, wide] of [
      [80 * rem - 1, false],
      [80 * rem, true],
    ] as const) {
      await page.setViewportSize({ width: workspace + chrome, height: 900 })
      await expect.poll(async () => Math.round((await sideBySide()).width)).toBe(workspace)
      await expect.poll(async () => (await sideBySide()).paired).toBe(wide)
      await expect.poll(async () => (await sideBySide()).breakdownRow).toBe(wide)
    }

    await page.setViewportSize({ width: 1440, height: 900 })
    await expect.poll(async () => (await sideBySide()).paired).toBe(false)
    await page.keyboard.press('Control+b')
    await expect.poll(async () => (await sideBySide()).paired).toBe(true)
    await page.keyboard.press('Control+b')
    await expect.poll(async () => (await sideBySide()).paired).toBe(false)
  })

  for (const locale of ['en-US', 'zh-CN']) {
    for (const theme of ['light', 'dark'] as const) {
      test(`activity and latency boundaries stay aligned without descriptions in ${locale} ${theme}`, async ({
        page,
      }) => {
        await prepareApp(page)
        await stubStats(page)
        await page.addInitScript(
          ({ locale, theme }) => {
            localStorage.setItem('stravia-locale', locale)
            localStorage.setItem('stravia-theme', theme)
          },
          { locale, theme },
        )
        await page.emulateMedia({ colorScheme: theme })
        await page.setViewportSize({ width: 1600, height: 900 })
        await page.goto('/stats')
        await expect(page.locator('html')).toHaveAttribute('lang', locale)
        const wrapper = page.locator('[data-activity-start]')
        await expect(wrapper.locator('.token-activity-cell').first()).toBeVisible()
        const latency = page.locator('[role="slider"]').filter({ has: page.locator('path[aria-label]') })
        await expect(latency).toBeVisible()
        await expect(async () => {
          const activityBox = (await wrapper.boundingBox())!
          const latencyBox = (await latency.boundingBox())!
          expect(Math.abs(activityBox.y - latencyBox.y)).toBeLessThanOrEqual(1)
          expect(Math.abs(activityBox.y + activityBox.height - latencyBox.y - latencyBox.height)).toBeLessThanOrEqual(1)
          expect(Math.abs(activityBox.height - 10 * rem)).toBeLessThanOrEqual(1)
        }).toPass()
        // Sections carry their title, not a descriptive paragraph.
        await expect(page.locator('.route-section-description')).toHaveCount(0)
      })
    }
  }

  for (const width of [390, 320]) {
    for (const locale of ['en-US', 'zh-CN']) {
      test(`${width}px keeps range, readings and every resource reachable without page overflow in ${locale}`, async ({
        page,
      }) => {
        await prepareApp(page)
        await stubStats(page)
        await populateResources(page)
        await page.addInitScript((locale) => localStorage.setItem('stravia-locale', locale), locale)
        await page.setViewportSize({ width, height: 844 })
        await page.goto('/stats')
        await expect(page.locator('html')).toHaveAttribute('lang', locale)
        const range = page.locator('.range-picker')
        await expect(range).toBeVisible()
        const rangeBox = (await range.boundingBox())!
        expect(rangeBox.x).toBeGreaterThanOrEqual(0)
        expect(rangeBox.x + rangeBox.width).toBeLessThanOrEqual(width)
        await page.locator('[role="slider"]').first().scrollIntoViewIfNeeded()
        await expect(page.locator('[data-activity-start]').locator('.token-activity-cell').first()).toBeVisible()
        await page.locator('.route-mobile-row').last().scrollIntoViewIfNeeded()
        await expect(page.locator('.route-mobile-row').last()).toBeVisible()
        expect(
          await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1),
        ).toBe(true)
        expect(await page.getByRole('main').evaluate((el) => el.scrollWidth <= el.clientWidth + 1)).toBe(true)
      })
    }
  }
})
