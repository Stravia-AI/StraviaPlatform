import { expect, test, type Page, type Route } from '@playwright/test'

import { prepareApp } from './prepare-app'

const HOUR = 3_600_000
const NOW = Date.UTC(2026, 9, 11, 12, 7)
const LIVE_END = Date.UTC(2026, 9, 11, 12, 15)
const START = LIVE_END - 24 * HOUR
const CURRENCY = { kind: 'usd' as const, symbol: '$' }
const knownMoney = { amount: '0.01', known_count: 1, unknown_count: 0 }

/**
 * 每桶计数：第 1 桶 100% / 90%，第 2 桶 100% / 100%，第 4 桶 95% / 85%，第 5 桶 92.5% / 80%。
 * 概览给同窗口的整数总量，但特意与序列求和不同：汇总必须读概览（90% / 85%，恢复 2 次）。
 */
const buckets = [
  { offset: 0, terminal: 10, failed: 0, recovered: 1, known: 10, upstream: 10, upstreamErrors: 0 },
  { offset: 1, terminal: 10, failed: 0, recovered: 0, known: 10, upstream: 10, upstreamErrors: 0 },
  // 2 点缺桶：两条线与差值带都必须在此断开，而不是补成 100%。
  { offset: 3, terminal: 40, failed: 2, recovered: 4, known: 40, upstream: 50, upstreamErrors: 5 },
  { offset: 4, terminal: 40, failed: 3, recovered: 5, known: 40, upstream: 50, upstreamErrors: 7 },
]

const overview = {
  total_requests: 100,
  terminal_request_count: 40,
  failed_request_count: 4,
  recovered_request_count: 2,
  recovery_known_request_count: 40,
  upstream_request_count: 120,
  upstream_error_count: 30,
  total_input_tokens: 600,
  total_output_tokens: 86,
  total_cache_read_tokens: 320,
  total_cache_write_tokens: 12,
  total_reasoning_tokens: null,
  avg_duration_ms: 120,
  avg_first_token_ms: 40,
  avg_output_tps: 25,
  currency: CURRENCY,
  actual_cost: knownMoney,
}

const providerRows = [
  {
    provider_id: 'provider-1',
    provider: 'Service 01',
    deleted: false,
    upstream_request_count: 120,
    upstream_error_count: 12,
    avg_duration_ms: 100,
    avg_output_tps: 10,
    currency: CURRENCY,
    cost: knownMoney,
    subscription: null,
  },
]

type Counts = {
  terminal: number
  failed: number
  recovered: number
  known: number
  upstream: number
  upstreamErrors: number
}

function seriesRow(start: number, counts: Counts) {
  return {
    bucket_ms: HOUR,
    bucket_start: start,
    request_count: counts.terminal,
    terminal_request_count: counts.terminal,
    failed_request_count: counts.failed,
    recovered_request_count: counts.recovered,
    recovery_known_request_count: counts.known,
    upstream_request_count: counts.upstream,
    upstream_error_count: counts.upstreamErrors,
    total_input_tokens: 10,
    total_output_tokens: 1,
    total_cache_read_tokens: 0,
    total_cache_write_tokens: 0,
    total_reasoning_tokens: null,
    avg_duration_ms: 120,
    avg_first_token_ms: 40,
    avg_output_tps: 25,
    currency: CURRENCY,
    actual_cost: knownMoney,
  }
}

/**
 * 统计桩：选中窗口的 series 返回带缺桶的计数；活动图与其它窗口返回空序列。
 * `overrides` 让单个用例替换某个端点或记录调用次数。
 */
async function stubStats(
  page: Page,
  overrides: {
    series?: (route: Route, url: URL) => Promise<boolean>
    rows?: typeof buckets
    overview?: Record<string, unknown>
    overviewError?: () => boolean
  } = {},
) {
  const requests: URL[] = []
  await prepareApp(page)
  await page.route('**/api/v1/stats/**', async (route) => {
    const url = new URL(route.request().url())
    requests.push(url)
    const start = Number(url.searchParams.get('start_at'))
    const end = Number(url.searchParams.get('end_at'))
    const endpoint = url.pathname.split('/').at(-1)
    if (endpoint === 'overview') {
      if (overrides.overviewError?.()) {
        await route.fulfill({ status: 500, json: { error: 'Summary unavailable' } })
        return
      }
      await route.fulfill({ json: { data: overrides.overview ?? overview } })
    } else if (endpoint === 'series') {
      if (overrides.series && (await overrides.series(route, url))) return
      if (start !== START || end !== LIVE_END || url.searchParams.get('bucket') !== '3600') {
        await route.fulfill({ json: { data: [] } })
        return
      }
      await route.fulfill({
        json: { data: (overrides.rows ?? buckets).map((bucket) => seriesRow(START + bucket.offset * HOUR, bucket)) },
      })
    } else if (endpoint === 'providers') {
      await route.fulfill({ json: { data: providerRows } })
    } else {
      await route.fulfill({ json: { data: [] } })
    }
  })
  return requests
}

async function availability(page: Page) {
  const region = page.getByRole('region', { name: 'Request success', exact: true })
  await expect(region).toBeVisible()
  return { region, slider: page.getByRole('slider', { name: 'Request success chart', exact: true }) }
}

test.describe('statistics request success chart', () => {
  test.use({ timezoneId: 'UTC' })

  test('draws final and first-pass lines with the fixed axis, recovery band and window aggregate', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await stubStats(page)
    await page.goto('/stats')
    const { region, slider } = await availability(page)
    await expect(slider).toBeVisible()

    // 图例头部：线型 + 名称 + 窗口聚合值（概览总量相除，不是序列求和）。
    const header = region.locator('.route-section-header')
    await expect(header.getByText('Final success rate', { exact: true })).toBeVisible()
    await expect(header.getByText('90%', { exact: true })).toBeVisible()
    await expect(header.getByText('First-pass success rate', { exact: true })).toBeVisible()
    await expect(header.getByText('85%', { exact: true })).toBeVisible()
    // 恢复次数与百分点提升只在提示框读取，图例头部不再显示。
    await expect(header).not.toContainText('recovered')
    // 序列求和的最终成功率不是 90%；图例不得显示它。
    await expect(header).not.toContainText('95%')

    await header.getByRole('button', { name: 'About request success', exact: true }).hover()
    const help = page.locator('[data-slot="tooltip-content"][data-state$="open"]')
    await expect(help).toBeVisible()

    // 固定 0–100% 轴，不随数据起伏缩放。
    const axis = region.getByRole('group', { name: 'Success rate axis', exact: true })
    await expect(axis).toBeVisible()
    await expect(axis.getByText('0%', { exact: true })).toBeVisible()
    await expect(axis.getByText('100%', { exact: true })).toBeVisible()

    const lines = region.locator('path[aria-label]')
    await expect(lines).toHaveCount(2)
    // 一次通过虚线先画、最终成功实线后画：重叠时实线在上方。
    expect(await lines.evaluateAll((els) => els.map((el) => el.getAttribute('aria-label')))).toEqual([
      'First-pass success rate',
      'Final success rate',
    ])
    const styles = await page.evaluate(() => {
      const style = (label: string) => {
        const element = document.querySelector(`path[aria-label="${label}"]`)
        return element ? getComputedStyle(element) : null
      }
      const final = style('Final success rate')
      const firstPass = style('First-pass success rate')
      return {
        final: { stroke: final?.stroke, dash: final?.strokeDasharray },
        firstPass: { stroke: firstPass?.stroke, dash: firstPass?.strokeDasharray },
      }
    })
    expect(styles.final.stroke).not.toBe(styles.firstPass.stroke)
    expect(styles.final.dash).toBe('none')
    expect(styles.firstPass.dash).not.toBe('none')

    // 差值带填充两线之间，并在缺桶处断开。
    const band = region.locator('path[data-slot="recovery-band"]')
    await expect(band).toHaveCount(1)
    expect(((await band.getAttribute('d'))!.match(/M/g) ?? []).length).toBe(2)
  })

  test('the window aggregate always comes from the overview totals', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    // 概览总量换成 80% / 70%、恢复 5 次；图例必须跟着概览，而不是序列求和。
    await stubStats(page, {
      overview: {
        ...overview,
        terminal_request_count: 50,
        failed_request_count: 10,
        recovered_request_count: 5,
        recovery_known_request_count: 50,
        upstream_request_count: 40,
        upstream_error_count: 20,
      },
    })
    await page.goto('/stats')
    const { region } = await availability(page)
    const header = region.locator('.route-section-header')
    await expect(header.getByText('80%', { exact: true })).toBeVisible()
    await expect(header.getByText('70%', { exact: true })).toBeVisible()
    // 折线仍按桶级请求计数绘制，与图例的数据来源无关。
    await expect(page.getByRole('slider', { name: 'Request success chart', exact: true })).toBeVisible()
  })

  test('zero recovery keeps both lines together and reports zero contribution', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await stubStats(page, {
      rows: buckets.map((bucket) => ({ ...bucket, recovered: 0 })),
      overview: { ...overview, recovered_request_count: 0 },
    })
    await page.goto('/stats')
    const { region, slider } = await availability(page)
    const header = region.locator('.route-section-header')
    // 没有自动恢复时两条线的图例读数相同。
    await expect(header.getByText('90%', { exact: true })).toHaveCount(2)

    // 没有自动恢复时两条线使用完全相同的点，差值带退化为零面积。
    const pathData = (label: string) => region.locator(`path[aria-label="${label}"]`).getAttribute('d')
    const finalPath = await pathData('Final success rate')
    expect(finalPath).toBeTruthy()
    expect(await pathData('First-pass success rate')).toBe(finalPath)

    await slider.focus()
    await expect(page.locator('.lc-tooltip-root')).toContainText('0 recovered · 0 pp')
  })

  test('a failed summary keeps the legend neutral while the series still draws', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    let overviewAttempts = 0
    await stubStats(page, {
      overviewError: () => {
        overviewAttempts += 1
        // 首次查询与 TanStack 的一次自动重试都失败，页面级部分失败提示才会出现。
        return overviewAttempts <= 2
      },
    })
    await page.goto('/stats')
    const { region } = await availability(page)
    const header = region.locator('.route-section-header')
    // 概览失败时聚合未知，显示中性的“—”，不把序列求和当成窗口值。
    await expect(header.getByText('—', { exact: true })).toHaveCount(2)
    await expect(page.getByRole('slider', { name: 'Request success chart', exact: true })).toBeVisible()
    // 沿用既有部分失败模式：页面级提示点名 Summary 并提供重试。
    await expect(page.getByText('Some usage data could not be refreshed')).toBeVisible()
    await expect(page.getByText('Could not refresh: Summary.', { exact: true })).toBeVisible()

    await page.getByRole('button', { name: 'Retry all', exact: true }).click()
    await expect.poll(() => overviewAttempts).toBeGreaterThan(2)
    await expect(header.getByText('90%', { exact: true })).toBeVisible()
    await expect(header.getByText('85%', { exact: true })).toBeVisible()
  })

  test('incomplete recovery coverage keeps final success but marks the recovery readings unknown', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await stubStats(page, {
      overview: { ...overview, recovery_known_request_count: 39 },
      rows: buckets.map((bucket) => ({ ...bucket, known: bucket.terminal - 1 })),
    })
    await page.goto('/stats')
    const { region, slider } = await availability(page)
    const header = region.locator('.route-section-header')
    // 最终成功率照常；一次通过率保持未知，说明在帮助提示与图表提示框中。
    await expect(header.getByText('90%', { exact: true })).toBeVisible()
    await expect(header.getByText('—', { exact: true })).toHaveCount(1)

    await slider.focus()
    const tooltip = page.locator('.lc-tooltip-root')
    await expect(tooltip).toContainText('Final success rate')
    await expect(tooltip).toContainText('First-pass success rate')
    await expect(tooltip).toContainText('Recovery tracking is incomplete for this bucket')

    await header.getByRole('button', { name: 'About request success', exact: true }).hover()
    await expect(page.locator('[data-slot="tooltip-content"][data-state$="open"]')).toContainText(
      'recovery tracking is incomplete',
    )
  })

  test('a bucket without traffic breaks both lines and the recovery band instead of interpolating', async ({
    page,
  }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await stubStats(page)
    await page.goto('/stats')
    const { region, slider } = await availability(page)
    await expect(slider).toHaveAttribute('aria-valuemax', '4')
    // 5 个桶中第 3 个缺桶：两条线与差值带各自重新起笔。
    for (const label of ['First-pass success rate', 'Final success rate']) {
      const path = region.locator(`path[aria-label="${label}"]`)
      const commands = (await path.getAttribute('d'))!.match(/M/g) ?? []
      expect(commands.length).toBe(2)
    }
    const band = region.locator('path[data-slot="recovery-band"]')
    expect(((await band.getAttribute('d'))!.match(/M/g) ?? []).length).toBe(2)

    await slider.focus()
    await page.keyboard.press('ArrowRight')
    await page.keyboard.press('ArrowRight')
    await expect(slider).toHaveAttribute('aria-valuenow', '2')
    await expect(slider).toHaveAttribute('aria-valuetext', /Final success rate: —/)
    await expect(slider).not.toHaveAttribute('aria-valuetext', /incomplete/i)
  })

  test('keyboard readout moves with arrows and Home/End and closes with Escape', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await stubStats(page)
    await page.goto('/stats')
    const { slider } = await availability(page)
    const tooltip = page.locator('.lc-tooltip-root')

    await slider.focus()
    await expect(tooltip).toContainText('Final success rate')
    await expect(tooltip).toContainText('0 failed of 10 terminal requests')
    await expect(tooltip).toContainText('1 recovered · +10 pp')
    await expect(tooltip).toContainText('Upstream attempts')
    await expect(tooltip).toContainText('0 upstream errors of 10 upstream requests')

    await page.keyboard.press('ArrowRight')
    await expect(slider).toHaveAttribute('aria-valuenow', '1')
    await page.keyboard.press('End')
    await expect(slider).toHaveAttribute('aria-valuenow', '4')
    await expect(tooltip).toContainText('92.5%')
    await expect(tooltip).toContainText('80%')
    await expect(tooltip).toContainText('3 failed of 40 terminal requests')
    await expect(tooltip).toContainText('5 recovered · +12.5 pp')
    await expect(tooltip).toContainText('7 upstream errors of 50 upstream requests')
    await expect(tooltip).toContainText('Per-attempt success: 86%')
    const valueText = await slider.getAttribute('aria-valuetext')
    expect(valueText).toContain('Final success rate: 92.5%')
    expect(valueText).toContain('First-pass success rate: 80%')

    await page.keyboard.press('Home')
    await expect(slider).toHaveAttribute('aria-valuenow', '0')
    await page.keyboard.press('Escape')
    await expect(tooltip).toHaveCount(0)
  })

  test('a window without terminal requests explains itself instead of drawing 100%', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    await stubStats(page, {
      rows: [{ offset: 0, terminal: 0, failed: 0, recovered: 0, known: 0, upstream: 0, upstreamErrors: 0 }],
      overview: {
        ...overview,
        total_requests: 3,
        terminal_request_count: 0,
        failed_request_count: 0,
        recovered_request_count: 0,
        recovery_known_request_count: 0,
        upstream_request_count: 0,
        upstream_error_count: 0,
      },
    })
    await page.goto('/stats')
    const region = page.getByRole('region', { name: 'Request success', exact: true })
    await expect(region.getByText('No request outcomes.', { exact: true })).toBeVisible()
    await expect(region.getByRole('slider')).toHaveCount(0)
    // 图例聚合保持中性，不把未知读成 100%。
    await expect(region.locator('.route-section-header').getByText('—', { exact: true })).toHaveCount(2)
  })

  test('a failed availability query keeps the other sections and retries only its own request', async ({ page }) => {
    await page.clock.setFixedTime(NOW)
    await page.setViewportSize({ width: 1600, height: 1000 })
    let seriesAttempts = 0
    const requests = await stubStats(page, {
      series: async (route, url) => {
        if (Number(url.searchParams.get('start_at')) !== START || url.searchParams.get('bucket') !== '3600')
          return false
        seriesAttempts += 1
        // 首次查询与 TanStack 的一次自动重试都失败，错误卡片才会出现。
        if (seriesAttempts <= 2) {
          await route.fulfill({ status: 500, json: { error: 'Availability unavailable' } })
          return true
        }
        return false
      },
    })
    await page.goto('/stats')
    const region = page.getByRole('region', { name: 'Request success', exact: true })
    await expect(region.getByRole('alert')).toContainText('Availability unavailable')
    await expect(region.getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
    // 其余数据仍然可用：指标条与供应商表保留，页面级提示说明哪部分没有刷新。
    await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Total Requests' })).toContainText('100')
    await expect(page.getByRole('region', { name: 'Providers', exact: true }).getByRole('table')).toBeVisible()
    await expect(page.getByText('Some usage data could not be refreshed')).toBeVisible()

    await region.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect.poll(() => seriesAttempts).toBeGreaterThan(1)
    await expect(region.getByRole('slider')).toBeVisible()
    expect(requests.filter((url) => url.pathname.endsWith('/overview')).length).toBeGreaterThanOrEqual(1)
  })

  test.describe('touch readout', () => {
    test.use({ hasTouch: true, isMobile: true, deviceScaleFactor: 2, viewport: { width: 320, height: 568 } })

    test('a touch click restores its bucket after native pointer-leave hiding', async ({ page }) => {
      await page.clock.setFixedTime(NOW)
      await stubStats(page)
      await page.goto('/stats')
      const { slider } = await availability(page)
      const marker = (await slider.locator('circle').last().boundingBox())!
      const pointer = {
        pointerType: 'touch',
        clientX: marker.x + marker.width / 2,
        clientY: marker.y + marker.height / 2,
      }
      const context = slider.locator('.lc-tooltip-context')
      await context.dispatchEvent('pointerenter', pointer)
      await context.dispatchEvent('pointerdown', pointer)
      const tooltip = page.locator('.lc-tooltip-root')
      await expect(tooltip).toContainText('92.5%')
      await context.dispatchEvent('pointerleave', pointer)
      await expect(tooltip).toHaveCount(0)

      await context.dispatchEvent('click', pointer)
      await expect(tooltip).toContainText('5 recovered · +12.5 pp')
      await expect(slider).toHaveAttribute('aria-valuenow', '4')
    })

    for (const locale of ['en-US', 'zh-CN'] as const) {
      test(`touch selects a zero-percent bucket at the plot boundary in ${locale}`, async ({ page }) => {
        await page.clock.setFixedTime(NOW)
        await stubStats(page, {
          rows: buckets.map((bucket, index) =>
            index === buckets.length - 1 ? { ...bucket, failed: bucket.terminal, recovered: 0 } : bucket,
          ),
        })
        await page.addInitScript((locale) => localStorage.setItem('stravia-locale', locale), locale)
        await page.goto('/stats')
        const chinese = locale === 'zh-CN'
        const slider = page.getByRole('slider', {
          name: chinese ? '请求成功率图' : 'Request success chart',
          exact: true,
        })
        await slider.locator('circle').last().tap()
        await expect(page.locator('.lc-tooltip-root')).toContainText(
          chinese ? '40 个已终结请求中 40 个失败' : '40 failed of 40 terminal requests',
        )
        await expect(slider).toHaveAttribute('aria-valuenow', '4')
      })

      test(`the first tap selects its bucket and keeps the tooltip inside 320px in ${locale}`, async ({ page }) => {
        await page.clock.setFixedTime(NOW)
        await stubStats(page)
        await page.addInitScript((locale) => localStorage.setItem('stravia-locale', locale), locale)
        await page.goto('/stats')
        const chinese = locale === 'zh-CN'
        const slider = page.getByRole('slider', {
          name: chinese ? '请求成功率图' : 'Request success chart',
          exact: true,
        })
        await expect(slider).toBeVisible()
        const beforeTap = await page.evaluate(() => document.documentElement.scrollWidth)
        await slider.locator('circle').last().tap()
        const tooltip = page.locator('.lc-tooltip-root')
        await expect(tooltip).toContainText('92.5%')
        await expect(tooltip).toContainText('80%')
        await expect(slider).toHaveAttribute('aria-valuenow', '4')
        const withTooltip = await page.evaluate(() => document.documentElement.scrollWidth)
        const box = (await tooltip.boundingBox())!
        expect(box.x).toBeGreaterThanOrEqual(0)
        expect(box.x + box.width).toBeLessThanOrEqual(320)
        expect(withTooltip).toBeLessThanOrEqual(beforeTap)
        await page.keyboard.press('ArrowLeft')
        await expect(slider).toHaveAttribute('aria-valuenow', '3')
      })
    }
  })

  for (const locale of ['en-US', 'zh-CN'] as const) {
    for (const width of [320, 500, 1600] as const) {
      test(`the chart keeps axes, aggregate and readout at ${width}px in ${locale}`, async ({ page }) => {
        await page.clock.setFixedTime(NOW)
        await page.setViewportSize({ width, height: 900 })
        await stubStats(page)
        await page.addInitScript((locale) => localStorage.setItem('stravia-locale', locale), locale)
        await page.goto('/stats')
        await expect(page.locator('html')).toHaveAttribute('lang', locale)
        const chinese = locale === 'zh-CN'
        const region = page.getByRole('region', { name: chinese ? '请求成功率' : 'Request success', exact: true })
        const slider = page.getByRole('slider', {
          name: chinese ? '请求成功率图' : 'Request success chart',
          exact: true,
        })
        await slider.scrollIntoViewIfNeeded()
        await expect(slider).toBeVisible()
        await expect(
          region.getByRole('group', { name: chinese ? '成功率轴' : 'Success rate axis', exact: true }),
        ).toBeVisible()
        const header = region.locator('.route-section-header')
        await expect(header.getByText(chinese ? '最终成功率' : 'Final success rate', { exact: true })).toBeVisible()
        await expect(
          header.getByText(chinese ? '一次通过率' : 'First-pass success rate', { exact: true }),
        ).toBeVisible()
        await expect(header.getByText('90%', { exact: true })).toBeVisible()
        await expect(header.getByText('85%', { exact: true })).toBeVisible()

        const sectionBox = (await region.boundingBox())!
        expect(sectionBox.x).toBeGreaterThanOrEqual(0)
        expect(sectionBox.x + sectionBox.width).toBeLessThanOrEqual(width + 1)
        expect(
          await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1),
        ).toBe(true)

        await slider.focus()
        await expect(page.locator('.lc-tooltip-root')).toContainText(chinese ? '最终成功率' : 'Final success rate')
      })
    }
  }
})
