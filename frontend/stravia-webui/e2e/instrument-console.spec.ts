import { expect, test, type Locator, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
  await page.setViewportSize({ width: 1280, height: 800 })
})

test('empty Chat offers service setup instead of a composer', async ({ page }) => {
  await page.goto('/')

  const main = page.getByRole('main')
  await expect(main.getByRole('heading', { name: 'New conversation', exact: true })).toBeVisible()
  await expect(main.getByRole('link', { name: 'Connect a model service' })).toBeVisible()
  await expect(main.getByRole('textbox', { name: 'Message' })).toHaveCount(0)
})

test('configured Chat offers a composer before the first request', async ({ page }) => {
  await stubConnectableConfiguration(page)
  await page.goto('/')

  const main = page.getByRole('main')
  await expect(main.getByRole('textbox', { name: 'Message', exact: true })).toBeEditable()
  await expect(main.getByRole('button', { name: 'API Key', exact: true })).toContainText('Client key')
})

test('Chat does not treat a failed configuration fetch as an empty instance', async ({ page }) => {
  let providerFetchFails = true
  await page.route('**/api/v1/providers', async (route) => {
    if (providerFetchFails) {
      await route.fulfill({ status: 500, json: { error: 'Configuration fetch failed' } })
      return
    }
    await route.fulfill({ json: { data: [] } })
  })
  await page.goto('/')

  const main = page.getByRole('main')
  await expect(main.getByRole('alert')).toContainText('Configuration fetch failed')
  await expect(main.getByRole('link', { name: 'Connect a model service' })).toHaveCount(0)

  providerFetchFails = false
  await main.getByRole('button', { name: 'Retry' }).click()
  await expect(main.getByRole('link', { name: 'Connect a model service' })).toBeVisible()
})

test('Usage analytics exposes a failed latency series and recovers without fabricating zero speed', async ({
  page,
}) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  let seriesFails = true
  await page.route('**/api/v1/stats/series**', async (route) => {
    if (seriesFails && new URL(route.request().url()).searchParams.get('bucket') === '3600') {
      await route.fulfill({ status: 500, json: { error: 'Time series unavailable' } })
      return
    }
    await route.fallback()
  })
  await page.goto('/stats')
  const latency = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Latency and speed', exact: true }) })
  const retry = latency.getByRole('button', { name: 'Retry', exact: true })
  await expect(retry).toBeEnabled()
  await expect(page.getByLabel('Latency and speed chart', { exact: true })).toHaveCount(0)
  await expect(page.getByText('0 tok/s', { exact: true })).toHaveCount(0)
  seriesFails = false
  await retry.click()
  await expect(page.getByLabel('Latency and speed chart', { exact: true })).toBeVisible()
  await expect(retry).toHaveCount(0)
})

test('Usage analytics with traffic shows latency with output speed', async ({ page }) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  await page.goto('/stats')

  const latency = page.locator('section').filter({ has: page.getByLabel('Latency and speed chart', { exact: true }) })
  await expect(latency.getByText('0.04 s', { exact: true })).toBeVisible()
  await expect(latency.getByText('25 tok/s', { exact: true })).toBeVisible()
  await expect(latency.getByLabel('Latency and speed chart', { exact: true })).toBeVisible()
  await expect(latency.getByText('Duration', { exact: true })).toHaveCount(0)
  await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Input Tokens' })).toContainText('600')
  await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Output Tokens' })).toContainText('86')
  await expect(page.getByText('Total Tokens', { exact: true })).toHaveCount(0)
})

for (const routePath of ['/stats']) {
  test(`${routePath} does not turn unknown latency and speed into a zero-valued chart`, async ({ page }) => {
    await stubTraffic(page, { requests: 12, errors: 0 })
    await page.route('**/api/v1/stats/overview**', async (route) => {
      await route.fulfill({ json: { data: latencyOverview(null, null) } })
    })
    await page.route('**/api/v1/stats/series**', async (route) => {
      await route.fulfill({ json: { data: [latencyBucket(Date.UTC(2026, 8, 26), 0, null, null)] } })
    })
    await page.goto(routePath)
    await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Total requests' })).toContainText('12')
    await expect(page.locator('[data-slot="skeleton"]')).toHaveCount(0)
    await expect(page.getByLabel('Latency and speed chart', { exact: true })).toHaveCount(0)
    await expect(page.getByText('0 tok/s', { exact: true })).toHaveCount(0)
    await expect(page.getByRole('button', { name: /Retry/ })).toHaveCount(0)
  })

  test(`${routePath} binds seconds and TPS to independent zero-based linear axes`, async ({ page }) => {
    await stubTraffic(page, { requests: 12, errors: 0 })
    const start = new Date(2026, 8, 26, 12).getTime()
    await page.clock.setFixedTime(start + 4 * 3_600_000 + 5 * 60_000)
    let firstTokenFactor = 1
    let tpsFactor = 1
    await page.route('**/api/v1/stats/series**', async (route) => {
      await route.fulfill({
        json: {
          data: [0, 1, 2, 3, 4].map((hour) =>
            latencyBucket(start, hour, (hour + 1) * 200 * firstTokenFactor, (hour + 1) * 10_000 * tpsFactor),
          ),
        },
      })
    })
    await page.goto(routePath)
    const chart = page.getByLabel('Latency and speed chart', { exact: true })
    await expect(chart).toBeVisible()
    const firstToken = chart.getByLabel('Time to first token', { exact: true })
    const tps = chart.getByLabel('TPS', { exact: true })
    const left = chart.getByLabel('Time to first token axis', { exact: true })
    const right = chart.getByLabel('TPS axis', { exact: true })
    await expect(left).toBeVisible()
    await expect(right).toBeVisible()
    await expect(chart.getByText('s', { exact: true })).toBeVisible()
    await expect(chart.getByText('tok/s', { exact: true })).toBeVisible()
    expect(await firstToken.evaluate((el) => getComputedStyle(el).strokeDasharray)).toBe('none')
    expect(await tps.evaluate((el) => getComputedStyle(el).strokeDasharray)).not.toBe('none')
    const baseline = await axisGeometry(chart)
    expect(baseline.leftTicks[0]).toBe(0)
    expect(baseline.rightTicks[0]).toBe(0)
    expect(baseline.leftTicks.at(-1)!).toBeGreaterThan(0.5)
    expect(baseline.rightTicks.at(-1)!).toBeGreaterThan(10_000)
    for (const [axis, value, curveY] of [
      [baseline.leftPositions, 0.2, baseline.firstTokenY],
      [baseline.rightPositions, 10_000, baseline.tpsY],
    ] as const) {
      const zero = axis[0]
      const upper = axis.at(-1)!
      // 真实数值按轴的线性刻度定位；不能仅配上独立标签却绘制归一化伪坐标。
      const expectedY = zero.y + ((upper.y - zero.y) * value) / upper.value
      expect(Math.abs(curveY - expectedY)).toBeLessThan(8)
    }
    const firstPoints = await curvePoints(firstToken)
    const tpsPoints = await curvePoints(tps)
    // 相同比例的原始数值应覆盖相同高度，而非让秒值被 TPS 的量级压扁。
    expect(Math.abs(firstPoints[0].y - tpsPoints[0].y)).toBeLessThan(1)
    expect(Math.abs(firstPoints.at(-1)!.y - tpsPoints.at(-1)!.y)).toBeLessThan(1)
    expect(Math.abs(firstPoints[0].y - firstPoints.at(-1)!.y)).toBeGreaterThan(80)
    await chart.focus()
    await chart.press('Home')
    await expect(page.getByRole('tooltip')).toContainText('0.2 s')
    await expect(page.getByRole('tooltip')).toContainText('10,000 tok/s')

    tpsFactor = 100
    await page.reload()
    await expect
      .poll(async () => (await axisGeometry(chart)).rightTicks.at(-1))
      .toBeGreaterThan(baseline.rightTicks.at(-1)! * 50)
    const tpsScaled = await axisGeometry(chart)
    expect(tpsScaled.leftTicks).toEqual(baseline.leftTicks)
    expect(tpsScaled.firstTokenPath).toBe(baseline.firstTokenPath)
    expect(tpsScaled.rightTicks).not.toEqual(baseline.rightTicks)

    firstTokenFactor = 1000
    await page.reload()
    await expect
      .poll(async () => (await axisGeometry(chart)).leftTicks.at(-1))
      .toBeGreaterThan(baseline.leftTicks.at(-1)! * 500)
    const bothScaled = await axisGeometry(chart)
    expect(bothScaled.rightTicks).toEqual(tpsScaled.rightTicks)
    expect(bothScaled.tpsPath).toBe(tpsScaled.tpsPath)
    expect(bothScaled.leftTicks).not.toEqual(tpsScaled.leftTicks)
  })

  test(`${routePath} retains known metrics, genuine zero speed and unconnected gaps`, async ({ page }) => {
    await stubTraffic(page, { requests: 12, errors: 0 })
    const start = new Date(2026, 8, 26, 12).getTime()
    await page.clock.setFixedTime(start + 9 * 3_600_000 + 5 * 60_000)
    let overviewFirstToken: number | null = null
    let overviewTps: number | null = 0
    await page.route('**/api/v1/stats/overview**', async (route) => {
      await route.fulfill({ json: { data: latencyOverview(overviewFirstToken, overviewTps) } })
    })
    await page.route('**/api/v1/stats/series**', async (route) => {
      await route.fulfill({
        json: {
          data: [
            latencyBucket(start, 0, 200, 10_000),
            latencyBucket(start, 1, null, 20_000),
            latencyBucket(start, 2, null, 30_000),
            latencyBucket(start, 3, 800, 40_000),
            latencyBucket(start, 4, 1000, null),
            latencyBucket(start, 5, 1200, null),
            latencyBucket(start, 6, 1400, 0),
            // 第七小时没有请求，不能把第六与第八小时连起来。
            latencyBucket(start, 8, 1800, 50_000),
            latencyBucket(start, 9, 2000, 0),
          ],
        },
      })
    })
    await page.goto(routePath)
    const chart = page.getByLabel('Latency and speed chart', { exact: true })
    await expect(chart).toBeVisible()
    const section = page.locator('section').filter({ has: chart })
    await expect(section.getByText('—', { exact: true })).toBeVisible()
    await expect(section.getByText('0 tok/s', { exact: true })).toBeVisible()
    const first = await curvePoints(chart.getByLabel('Time to first token', { exact: true }))
    const speed = await curvePoints(chart.getByLabel('TPS', { exact: true }))
    const pitch = (first.at(-1)!.x - first[0].x) / 9
    const nearHour = (points: Array<{ x: number; y: number }>, hour: number) =>
      points.filter(({ x }) => Math.abs(x - first[0].x - pitch * hour) < pitch * 0.2)
    for (const hour of [1, 2, 7]) expect(nearHour(first, hour)).toHaveLength(0)
    for (const hour of [4, 5, 7]) expect(nearHour(speed, hour)).toHaveLength(0)
    expect(nearHour(speed, 1).length).toBeGreaterThan(0)
    expect(nearHour(first, 4).length).toBeGreaterThan(0)
    const zero = nearHour(speed, 9)
    expect(zero.length).toBeGreaterThan(0)
    expect(Math.max(...speed.map(({ y }) => y)) - zero.at(-1)!.y).toBeLessThan(1)
    await chart.focus()
    await chart.press('Home')
    await chart.press('ArrowRight')
    await expect(page.getByRole('tooltip')).toContainText('—')
    await expect(page.getByRole('tooltip')).toContainText('20,000 tok/s')
    for (let hour = 1; hour < 4; hour++) await chart.press('ArrowRight')
    await expect(page.getByRole('tooltip')).toContainText('1 s')
    await expect(page.getByRole('tooltip')).toContainText('—')
    await chart.press('ArrowRight')
    await chart.press('ArrowRight')
    await expect(page.getByRole('tooltip')).toContainText('1.4 s')
    await expect(page.getByRole('tooltip')).toContainText('0 tok/s')
    overviewFirstToken = 400
    overviewTps = null
    await page.reload()
    await expect(section.getByText('0.4 s', { exact: true })).toBeVisible()
    await expect(section.getByText('—', { exact: true })).toBeVisible()
    await expect(section.getByText('0 tok/s', { exact: true })).toHaveCount(0)
  })
  test(`${routePath} latency stays chronological across repeated clock labels`, async ({ page }) => {
    await stubTraffic(page, { requests: 22, errors: 0 })
    const start = new Date(2026, 8, 26, 19).getTime()
    await page.clock.setFixedTime(new Date(start + 24 * 3_600_000 + 5 * 60_000))
    await page.route('**/api/v1/stats/series**', async (route) => {
      const data = Array.from({ length: 25 }, (_, hour) => ({
        bucket_start: start + hour * 3_600_000,
        request_count: 1,
        error_count: 0,
        total_input_tokens: 10,
        total_output_tokens: 5,
        total_cache_read_tokens: 0,
        total_cache_write_tokens: 0,
        total_reasoning_tokens: 0,
        avg_duration_ms: 5_000 + (hour % 5) * 1_000,
        avg_first_token_ms: 1_000 + (hour % 3) * 500,
        avg_output_tps: 10_000 + (hour % 5) * 2_000,
      })).filter((_, hour) => ![7, 13, 14].includes(hour))
      await route.fulfill({ json: { data } })
    })
    await page.goto(routePath)

    const chart = page.getByLabel('Latency and speed chart', { exact: true })
    await expect(chart).toBeVisible()
    for (const metric of ['Time to first token', 'TPS']) {
      const curve = chart.getByLabel(metric, { exact: true })
      await expect
        .poll(async () =>
          curve.evaluate((element) => {
            const path = element as SVGPathElement
            const length = path.getTotalLength()
            if (length === 0) return false
            let previous = path.getPointAtLength(0).x
            for (let sample = 1; sample <= 200; sample++) {
              const x = path.getPointAtLength((length * sample) / 200).x
              if (x < previous - 0.01) return false
              previous = x
            }
            return previous > path.getPointAtLength(0).x
          }),
        )
        .toBe(true)
      const points = await curvePoints(curve)
      const pitch = (points.at(-1)!.x - points[0].x) / 24
      expect(pitch).toBeGreaterThan(0)
      for (const missingHour of [7, 13, 14]) {
        expect(points.some(({ x }) => Math.abs(x - points[0].x - pitch * missingHour) < pitch * 0.3)).toBe(false)
      }
    }
  })
}

test.describe('touch metric explanations', () => {
  test.use({ hasTouch: true })
  for (const routePath of ['/stats']) {
    test(`${routePath} touch opens and dismisses the latency definition`, async ({ page }) => {
      await stubTraffic(page, { requests: 12, errors: 0 })
      await page.setViewportSize({ width: 320, height: 800 })
      await page.goto(routePath)
      const latency = page
        .locator('section')
        .filter({ has: page.getByLabel('Latency and speed chart', { exact: true }) })
      await expect(latency).toBeVisible()
      await latency.getByRole('button', { name: 'About latency and speed' }).tap()
      const explanation = page.getByRole('tooltip')
      await expect(explanation).toBeVisible()
      await expect(explanation).toContainText('Thinking')
      await expect(explanation).toContainText('TPS')
      await latency.getByRole('heading').tap()
      await expect(explanation).toHaveCount(0)
    })
  }
})

test('Usage analytics preserves range choices and hourly latency buckets', async ({ page }) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  const queries: URL[] = []
  await page.route('**/api/v1/stats/series**', async (route) => {
    const query = new URL(route.request().url())
    // Token 活动图有独立且随窗口变化的粒度；这里只检查双轴折线的小时桶。
    if (query.searchParams.get('bucket') !== '3600') {
      await route.fallback()
      return
    }
    queries.push(query)
    const hours = Number(query.searchParams.get('hours'))
    await route.fulfill({
      json: { data: [latencyBucket(Math.floor(Date.now() / 3_600_000) * 3_600_000, 0, hours * 1000, hours * 10)] },
    })
  })
  await page.goto('/stats')
  const selector = page.locator('[data-slot="select-trigger"]')
  for (const [label, hours] of [
    ['Last 6h', 6],
    ['Last 24h', 24],
    ['Last 3d', 72],
    ['Last 7d', 168],
  ] as const) {
    await selector.click()
    await page.getByRole('option', { name: label, exact: true }).click()
    await expect(selector).toContainText(label)
    const chart = page.getByLabel('Latency and speed chart', { exact: true })
    await expect(chart).toBeVisible()
    await chart.focus()
    await chart.press('Home')
    await expect(page.getByRole('tooltip')).toContainText(`${hours} s`)
    const speed = page.getByRole('tooltip').locator('dd').nth(1)
    await expect.poll(async () => parseFloat((await speed.innerText()).replaceAll(',', ''))).toBe(hours * 10)
    await expect(speed).toContainText('tok/s')
    expect(queries.some((query) => Number(query.searchParams.get('hours')) === hours)).toBe(true)
  }
})

for (const routePath of ['/stats']) {
  test(`${routePath} refreshes chart summary and series after thirty seconds`, async ({ page }) => {
    await page.clock.install({ time: new Date(2026, 8, 26, 19, 5) })
    await page.clock.pauseAt(new Date(2026, 8, 26, 19, 5, 1))
    await stubTraffic(page, { requests: 12, errors: 0 })
    let overviewReads = 0
    let seriesReads = 0
    await page.route('**/api/v1/stats/overview**', async (route) => {
      if (new URL(route.request().url()).searchParams.get('hours') !== '24') {
        await route.fallback()
        return
      }
      const refreshed = overviewReads++ > 0
      await route.fulfill({ json: { data: latencyOverview(refreshed ? 2000 : 1000, refreshed ? 50 : 25) } })
    })
    await page.route('**/api/v1/stats/series**', async (route) => {
      if (new URL(route.request().url()).searchParams.get('bucket') !== '3600') {
        await route.fallback()
        return
      }
      const refreshed = seriesReads++ > 0
      await route.fulfill({
        json: {
          data: [latencyBucket(new Date(2026, 8, 26, 19).getTime(), 0, refreshed ? 2000 : 1000, refreshed ? 50 : 25)],
        },
      })
    })
    await page.goto(routePath)
    const latency = page.locator('section').filter({ has: page.getByLabel('Latency and speed chart', { exact: true }) })
    await expect(latency.getByText('1 s', { exact: true })).toBeVisible()
    await expect(latency.getByText('25 tok/s', { exact: true })).toBeVisible()
    await page.clock.fastForward(29_000)
    await expect(latency.getByText('1 s', { exact: true })).toBeVisible()
    await expect(latency.getByText('25 tok/s', { exact: true })).toBeVisible()
    await page.clock.fastForward(1000)
    await expect(latency.getByText('2 s', { exact: true })).toBeVisible()
    await expect(latency.getByText('50 tok/s', { exact: true })).toBeVisible()
    const chart = latency.getByLabel('Latency and speed chart', { exact: true })
    await chart.focus()
    await chart.press('Home')
    await expect(page.getByRole('tooltip')).toContainText('2 s')
    await expect(page.getByRole('tooltip')).toContainText('50 tok/s')
  })
}

for (const locale of ['en-US', 'zh-CN']) {
  for (const theme of ['light', 'dark']) {
    test(`latency axes, summaries, tooltips and explanations remain readable at 320px in ${locale} ${theme}`, async ({
      page,
    }) => {
      await stubTraffic(page, { requests: 12, errors: 0 })
      await page.addInitScript(
        ({ locale, theme }) => {
          localStorage.setItem('stravia-locale', locale)
          localStorage.setItem('stravia-theme', theme)
        },
        { locale, theme },
      )
      await page.emulateMedia({ colorScheme: theme as 'light' | 'dark' })
      await page.setViewportSize({ width: 320, height: 800 })
      for (const path of ['/stats']) {
        await page.goto(path)
        await expect(page.locator('html')).toHaveAttribute('lang', locale)
        const chart = page.getByRole('slider').filter({ has: page.locator('path[aria-label="TPS"]') })
        await expect(chart).toBeVisible()
        await expect(chart.getByText('s', { exact: true })).toBeVisible()
        await expect(chart.getByText('tok/s', { exact: true })).toBeVisible()
        const section = page.locator('section').filter({ has: chart })
        await expect(section.getByText('0.04 s', { exact: true })).toBeVisible()
        await expect(section.getByText('25 tok/s', { exact: true })).toBeVisible()
        const axes = chart.locator('[role="group"][aria-label]').filter({ has: page.locator('text') })
        await expect(axes).toHaveCount(2)
        for (const axis of await axes.all()) {
          await expect(axis).toBeVisible()
          const bounds = await axis.boundingBox()
          expect(bounds!.x).toBeGreaterThanOrEqual(0)
          expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(320)
        }
        expect(await page.getByRole('main').evaluate((element) => element.scrollWidth <= element.clientWidth + 1)).toBe(
          true,
        )
        await chart.focus()
        for (const key of ['Home', 'End']) {
          await chart.press(key)
          const tooltip = page.getByRole('tooltip')
          await expect(tooltip).toBeVisible()
          await expect(tooltip).toContainText('tok/s')
          const bounds = await tooltip.boundingBox()
          expect(bounds!.x).toBeGreaterThanOrEqual(0)
          expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(320)
        }
        await chart.hover()
        const dataTooltip = page.getByRole('tooltip')
        await expect(dataTooltip).toBeVisible()
        const dataBounds = await dataTooltip.boundingBox()
        expect(dataBounds!.x).toBeGreaterThanOrEqual(0)
        expect(dataBounds!.x + dataBounds!.width).toBeLessThanOrEqual(320)
        await chart.press('Escape')
        const help = section.getByRole('button')
        for (const trigger of await help.all()) {
          await trigger.focus()
          await expect(page.getByRole('tooltip')).toBeVisible()
          const text = await page.getByRole('tooltip').innerText()
          expect(text).toContain('Thinking')
          expect(text).toContain('TPS')
          expect(text).toMatch(/首字|first.token/i)
          const tooltipBounds = await page.getByRole('tooltip').boundingBox()
          expect(tooltipBounds!.x).toBeGreaterThanOrEqual(0)
          expect(tooltipBounds!.x + tooltipBounds!.width).toBeLessThanOrEqual(320)
          await page.keyboard.press('Escape')
        }
        expect(await help.count()).toBeGreaterThanOrEqual(1)
      }
    })
  }
}

test('Usage analytics uses backend input and output without re-counting cache or reasoning', async ({ page }) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  await page.goto('/stats')

  for (const [label, value] of [
    ['Input Tokens', '600'],
    ['Output Tokens', '86'],
    ['Cache read tokens', '320'],
    ['Cache write tokens', '12'],
  ]) {
    await expect(
      page.locator('.route-metric-strip__item').filter({ has: page.getByText(label, { exact: true }) }),
    ).toContainText(value)
  }
  await expect(page.getByText('Reasoning', { exact: true })).toHaveCount(0)

  const apiKeyUsage = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'API Key usage', exact: true }) })
  const apiKeyTable = apiKeyUsage.getByRole('table', { name: 'API Key usage' })
  await expect(apiKeyTable).toContainText('600')
  await expect(apiKeyTable).toContainText('86')
  await expect(apiKeyTable).toContainText('320')
  await expect(apiKeyTable).toContainText('12')
  await expect(apiKeyTable).not.toContainText('Reasoning')

  await page.setViewportSize({ width: 390, height: 800 })
  const mobileApiKeyUsage = apiKeyUsage.locator('.route-mobile-list')
  await expect(mobileApiKeyUsage).toContainText('Input 600 · Output 86 · Cache read 320 · Cache write 12')
  await expect(mobileApiKeyUsage).not.toContainText('RSN')

  const latency = page.locator('section').filter({ has: page.getByLabel('Latency and speed chart', { exact: true }) })
  await expect(latency.getByText('0.04 s', { exact: true })).toBeVisible()
  await expect(latency.getByText('25 tok/s', { exact: true })).toBeVisible()
  await expect(latency.getByText('Duration', { exact: true })).toHaveCount(0)
  await expect(latency.getByLabel('Latency and speed chart', { exact: true })).toBeVisible()
})

test('Usage analytics finishes its first load when breakdowns arrive before the summary', async ({ page }) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  let releaseOverview!: () => void
  const overviewGate = new Promise<void>((resolve) => {
    releaseOverview = resolve
  })
  await page.route('**/api/v1/stats/overview**', async (route) => {
    await overviewGate
    await route.fallback()
  })
  const breakdowns = ['series', 'providers', 'api-keys', 'models'].map((name) =>
    page.waitForResponse((response) => new URL(response.url()).pathname === `/api/v1/stats/${name}`),
  )
  await page.goto('/stats')
  await Promise.all(breakdowns)
  await expect(page.locator('[data-slot="skeleton"]').first()).toBeVisible()
  await expect(page.getByLabel('Latency and speed chart', { exact: true })).toHaveCount(0)
  // 让已返回的分项查询完成订阅通知，再交付汇总，模拟桌面 IPC 的乱序完成。
  await page.evaluate(
    () => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))),
  )
  releaseOverview()
  await expect(page.getByRole('group', { name: 'Token activity' })).toBeVisible()
  await expect(page.getByLabel('Latency and speed chart', { exact: true })).toBeVisible()
  await expect(page.locator('[data-slot="skeleton"]')).toHaveCount(0)
})

test('Usage analytics exposes an initial query failure and recovers on retry', async ({ page }) => {
  await stubTraffic(page, { requests: 12, errors: 0 })
  let seriesFails = true
  await page.route('**/api/v1/stats/series**', async (route) => {
    if (seriesFails) {
      await route.fulfill({ status: 500, json: { error: 'Time series unavailable' } })
      return
    }
    await route.fallback()
  })
  await page.goto('/stats')

  const retryAll = page.getByRole('button', { name: 'Retry all' })
  await expect(retryAll).toBeEnabled()
  await expect(page.locator('.route-metric-strip__item').filter({ hasText: 'Total requests' })).toContainText('12')
  await expect(page.getByRole('group', { name: 'Token activity' })).toHaveCount(0)
  await expect(page.getByLabel('Latency and speed chart', { exact: true })).toHaveCount(0)

  seriesFails = false
  await retryAll.click()
  await expect(page.getByRole('group', { name: 'Token activity' })).toBeVisible()
  await expect(page.getByLabel('Latency and speed chart', { exact: true })).toBeVisible()
  await expect(retryAll).toHaveCount(0)
})

for (const scenario of [
  { path: '/api-keys', primary: '/api-keys', dependency: '/models', action: 'Create first API Key' },
  { path: '/models', primary: '/models', dependency: '/providers', action: 'Go to model services' },
]) {
  test(`${scenario.path} finishes its first load when dependencies arrive first`, async ({ page }) => {
    let releasePrimary!: () => void
    const primaryGate = new Promise<void>((resolve) => {
      releasePrimary = resolve
    })
    await page.route(`**/api/v1${scenario.primary}`, async (route) => {
      await primaryGate
      await route.fallback()
    })
    const dependency = page.waitForResponse(
      (response) => new URL(response.url()).pathname === `/api/v1${scenario.dependency}`,
    )
    await page.goto(scenario.path)
    await (await dependency).finished()
    await expect(page.locator('[data-slot="skeleton"]').first()).toBeVisible()
    // 先交付依赖并完成订阅通知，再让列表请求结束，防止偶然的响应顺序掩盖回归。
    await page.evaluate(
      () => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))),
    )
    const primary = page.waitForResponse(
      (response) => new URL(response.url()).pathname === `/api/v1${scenario.primary}`,
    )
    releasePrimary()
    await (await primary).finished()
    await expect(page.getByText(scenario.action, { exact: true })).toBeVisible()
    await expect(page.locator('[data-slot="skeleton"]')).toHaveCount(0)
  })

  test(`${scenario.path} recovers from a dependency failure after an ordered retry`, async ({ page }) => {
    let failing = true
    let releaseDependency!: () => void
    const dependencyGate = new Promise<void>((resolve) => {
      releaseDependency = resolve
    })
    await page.route(`**/api/v1${scenario.dependency}`, async (route) => {
      if (failing) {
        await route.fulfill({ status: 500, json: { error: 'Dependency unavailable' } })
        return
      }
      await dependencyGate
      await route.fallback()
    })
    await page.goto(scenario.path)
    const retry = page.getByRole('button', { name: 'Retry', exact: true })
    await expect(retry).toBeEnabled()
    await expect(page.getByText(scenario.action, { exact: true })).toHaveCount(0)

    failing = false
    const primary = page.waitForResponse(
      (response) => new URL(response.url()).pathname === `/api/v1${scenario.primary}`,
    )
    await retry.click()
    await (await primary).finished()
    await expect(page.getByText(scenario.action, { exact: true })).toHaveCount(0)
    releaseDependency()
    await expect(page.getByText(scenario.action, { exact: true })).toBeVisible()
    await expect(retry).toHaveCount(0)
    await expect(page.locator('[data-slot="skeleton"]')).toHaveCount(0)
  })
}

test('empty Model services, Models, API Keys, and logs speak the missing dependency', async ({ page }) => {
  await page.goto('/providers')
  await expect(page.getByText('A model has nowhere to go until you connect a model service.')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Connect first service' })).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Connect service' })).toHaveCount(0)

  await page.goto('/models')
  await expect(page.getByText('Connect a model service first')).toBeVisible()
  await expect(page.getByRole('link', { name: 'Go to model services' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Add model' })).toHaveCount(0)

  await page.goto('/api-keys')
  await expect(page.getByRole('button', { name: 'Create first API Key' })).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Create API Key' })).toHaveCount(0)

  await page.goto('/logs')
  await expect(page.getByRole('button', { name: 'Clear history' })).toBeEnabled()
  await expect(page.getByRole('tab', { name: 'Interaction Chains' })).toBeVisible()
})

test('Connect keeps client selection available while exposing missing resource recovery', async ({ page }) => {
  await page.goto('/connect')

  await expect(page.locator('#cli-tool')).toBeEnabled()
  await expect(page.locator('#cli-key')).toBeDisabled()
  await expect(page.getByRole('button', { name: 'Go to models', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Create an API Key', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Create API Key', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(page.getByRole('link', { name: 'Continue setup', exact: true })).toBeVisible()
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

  const rpm = page.locator('#api-key-rpm-limit')
  const expiresAt = page.getByLabel('Expires at')
  await expect(rpm).toBeVisible()
  await expect(expiresAt).toBeVisible()
  const rpmBox = await rpm.boundingBox()
  const expiresAtBox = await expiresAt.boundingBox()
  expect(rpmBox?.width ?? 0).toBeLessThan(nameBox?.width ?? 0)
  expect(Math.abs((rpmBox?.width ?? 0) - (expiresAtBox?.width ?? 0))).toBeLessThan(1)
  expect(Math.abs((rpmBox?.y ?? 0) - (expiresAtBox?.y ?? 0))).toBeLessThan(1)
  const help = overlay.getByRole('group').filter({ has: rpm }).getByRole('button', { name: 'More about this field' })
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
            rpm_limit: null,
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

function latencyOverview(firstToken: number | null, tps: number | null) {
  return {
    total_requests: 12,
    total_input_tokens: 920,
    total_output_tokens: 86,
    total_cache_read_tokens: 320,
    total_cache_write_tokens: 12,
    total_reasoning_tokens: 44,
    avg_duration_ms: 120,
    avg_first_token_ms: firstToken,
    avg_output_tps: tps,
    error_count: 0,
  }
}

function latencyBucket(start: number, hour: number, firstToken: number | null, tps: number | null) {
  return {
    bucket_start: start + hour * 3_600_000,
    request_count: 1,
    error_count: 0,
    total_input_tokens: 10,
    total_output_tokens: 5,
    total_cache_read_tokens: 0,
    total_cache_write_tokens: 0,
    total_reasoning_tokens: 0,
    avg_duration_ms: 2000,
    avg_first_token_ms: firstToken,
    avg_output_tps: tps,
  }
}

async function curvePoints(curve: Locator): Promise<Array<{ x: number; y: number }>> {
  return curve.evaluate((element) => {
    const path = element as SVGPathElement
    const length = path.getTotalLength()
    return Array.from({ length: 2001 }, (_, i) => {
      const point = path.getPointAtLength((length * i) / 2000)
      return { x: point.x, y: point.y }
    })
  })
}

async function axisGeometry(chart: Locator) {
  return chart.evaluate((element) => {
    const ticks = (label: string) => {
      const axis = element.querySelector(`[aria-label="${label}"]`)!
      return Array.from(axis.querySelectorAll('text'))
        .flatMap((text) => {
          const match = text.textContent
            ?.trim()
            .replaceAll(',', '')
            .match(/^(\d+(?:\.\d+)?)\s*([kMG])?(?:\s+(?:s|tok\/s))?$/)
          if (!match) return []
          const multipliers: Record<string, number> = { k: 1000, M: 1_000_000, G: 1_000_000_000 }
          const bounds = text.getBoundingClientRect()
          return [{ value: Number(match[1]) * (multipliers[match[2] ?? ''] ?? 1), y: bounds.y + bounds.height / 2 }]
        })
        .sort((a, b) => a.value - b.value)
    }
    const firstY = (label: string) => {
      const path = element.querySelector<SVGPathElement>(`[aria-label="${label}"]`)!
      const point = path.getPointAtLength(0)
      return new DOMPoint(point.x, point.y).matrixTransform(path.getScreenCTM()!).y
    }
    const leftPositions = ticks('Time to first token axis')
    const rightPositions = ticks('TPS axis')
    return {
      leftTicks: leftPositions.map(({ value }) => value),
      rightTicks: rightPositions.map(({ value }) => value),
      leftPositions,
      rightPositions,
      firstTokenY: firstY('Time to first token'),
      tpsY: firstY('TPS'),
      firstTokenPath: element.querySelector('[aria-label="Time to first token"]')!.getAttribute('d'),
      tpsPath: element.querySelector('[aria-label="TPS"]')!.getAttribute('d'),
    }
  })
}

async function stubConnectableConfiguration(page: Page): Promise<void> {
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'provider-openai',
            name: 'OpenAI',
            protocol: 'openai-compatible',
            base_url: 'https://api.openai.example/v1',
            use_proxy: false,
            is_enabled: true,
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-01T00:00:00Z',
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'gpt-5',
            model_id: 'gpt-5',
            display_name: 'GPT-5',
            balance: 'traffic_equalization',
            target_provider: 'provider-openai',
            target_model: 'gpt-5',
            is_enabled: true,
            created_at: '2026-01-01T00:00:00Z',
            supported_thinking_levels: [],
            targets: [
              {
                id: 'target-gpt',
                model_id: 'gpt-5',
                provider_id: 'provider-openai',
                model: 'gpt-5',
                enabled: true,
                priority: 0,
                first_token_timeout_ms: 5_000,
                target_retry_budget: 0,
                target_cooldown_ms: 0,
                created_at: '2026-01-01T00:00:00Z',
                thinking_level_map: [],
              },
            ],
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'key-client',
            key: 'sk-overview-client',
            name: 'Client key',
            rpm_limit: null,
            is_enabled: true,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_web_search: false,
            inject_media_understanding: false,
            expires_at: null,
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-01T00:00:00Z',
            model_ids: ['gpt-5'],
          },
        ],
      },
    })
  })
}

async function stubTraffic(page: Page, counts: { requests: number; errors: number }): Promise<void> {
  await page.route('**/api/v1/stats/overview**', async (route) => {
    await route.fulfill({
      json: {
        data: {
          total_requests: counts.requests,
          total_input_tokens: 600,
          total_output_tokens: 86,
          total_cache_read_tokens: 320,
          total_cache_write_tokens: 12,
          total_reasoning_tokens: 44,
          avg_duration_ms: 120,
          avg_first_token_ms: 40,
          avg_output_tps: 25,
          error_count: counts.errors,
        },
      },
    })
  })
  await page.route('**/api/v1/stats/series**', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            bucket_start: Date.UTC(2026, 7, 26),
            request_count: counts.requests,
            error_count: counts.errors,
            total_input_tokens: 600,
            total_output_tokens: 86,
            total_cache_read_tokens: 320,
            total_cache_write_tokens: 12,
            total_reasoning_tokens: 44,
            avg_duration_ms: 120,
            avg_first_token_ms: 40,
            avg_output_tps: 25,
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/stats/models**', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            model: 'gpt-5',
            request_count: counts.requests,
            total_input_tokens: 600,
            total_output_tokens: 86,
            total_reasoning_tokens: 44,
            avg_duration_ms: 120,
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/stats/api-keys**', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            api_key_id: 'key-1',
            api_key_name: 'Desktop client',
            request_count: counts.requests,
            total_input_tokens: 600,
            total_output_tokens: 86,
            cache_read_tokens: 320,
            cache_write_tokens: 12,
            reasoning_tokens: 44,
            last_used_at: Date.UTC(2026, 7, 26),
          },
        ],
      },
    })
  })
}
