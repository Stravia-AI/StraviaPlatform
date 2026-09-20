import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

async function stubStats(page: Page): Promise<void> {
  const now = new Date(2026, 8, 17, 8, 7).getTime()
  await page.clock.setFixedTime(now)
  await page.route('**/api/v1/stats/overview**', async (route) => {
    await route.fulfill({
      json: {
        data: {
          total_requests: 128,
          total_input_tokens: 190_000_000,
          total_output_tokens: 8_600_000,
          total_cache_read_tokens: 320_000,
          total_cache_write_tokens: 12_000,
          total_reasoning_tokens: 44_000,
          avg_duration_ms: 120,
          avg_first_token_ms: 40,
          error_count: 2,
        },
      },
    })
  })
  await page.route('**/api/v1/stats/series**', async (route) => {
    const url = new URL(route.request().url())
    const bucketMs = Number(url.searchParams.get('bucket') ?? '3600') * 1000
    const tzOffsetMs = Number(url.searchParams.get('tz_offset') ?? '0') * 1000
    const align = (t: number) => Math.floor((t + tzOffsetMs) / bucketMs) * bucketMs - tzOffsetMs
    const data = []
    for (let i = 0; i < 12; i++) {
      const start = align(now - i * bucketMs)
      data.push({
        bucket_start: start,
        request_count: 4 + i,
        error_count: i === 3 ? 1 : 0,
        total_input_tokens: i === 5 ? null : i === 0 ? 190_000_000 : 12_000 + i * 37_000,
        total_output_tokens: i === 5 ? null : 800 + i * 1_900,
        total_cache_read_tokens: i === 5 ? null : i * 700,
        total_cache_write_tokens: i === 5 ? null : i * 90,
        total_reasoning_tokens: i * 40,
        avg_duration_ms: 100 + i * 8,
        avg_first_token_ms: 30 + i,
      })
    }
    await route.fulfill({ json: { data } })
  })
}

// 粒度跟随范围选择器：6h→15 分钟，24h→1 小时，3 天→6 小时，7 天→1 天。
// 列数由容器宽度决定：网格向前延伸整列填满区域宽度但不溢出，行数仍由粒度决定。
test('token activity grid fills the section width at each granularity', async ({ page }) => {
  await prepareApp(page)
  await stubStats(page)
  await page.setViewportSize({ width: 1280, height: 900 })

  const ranges = [
    { value: '6', label: 'Last 6h', rows: 4, currentRows: 1 },
    { value: '24', label: 'Last 24h', rows: 6, currentRows: 3 },
    { value: '72', label: 'Last 3d', rows: 4, currentRows: 2 },
    { value: '168', label: 'Last 7d', rows: 7, currentRows: 4 },
  ]
  for (const range of ranges) {
    await page.goto('/stats')
    await page.locator('[data-slot="select-trigger"]').click()
    await page.getByRole('option', { name: range.label }).click()
    const grid = page.getByRole('group', { name: 'Token activity' })
    await expect(grid).toBeVisible()
    // 方格尺寸由容器高度推导，等待测量后的重排稳定再断言。
    await expect(async () => {
      const metrics = await grid.evaluate((el) => {
        const wrapper = el.parentElement!.parentElement!
        const cell = el.querySelector('.token-activity-cell')!.getBoundingClientRect().width
        const gridWidth = el.getBoundingClientRect().width
        return {
          cols: Math.floor((wrapper.clientWidth + 4) / (cell + 4)),
          cells: el.querySelectorAll('.token-activity-cell').length,
          leftover: wrapper.clientWidth - gridWidth,
          pitch: cell + 4,
          scrolls: wrapper.scrollWidth > wrapper.clientWidth + 1 || wrapper.scrollHeight > wrapper.clientHeight + 1,
        }
      })
      expect(metrics.scrolls).toBe(false)
      expect(metrics.leftover).toBeGreaterThanOrEqual(0)
      expect(metrics.leftover).toBeLessThan(metrics.pitch)
      expect(metrics.cells).toBe((metrics.cols - 1) * range.rows + range.currentRows)
    }).toPass()
  }
})

test('token activity cell tooltip reports the bucket time and token total', async ({ page }) => {
  await prepareApp(page)
  await stubStats(page)
  await page.goto('/stats')

  const grid = page.getByRole('group', { name: 'Token activity' })
  await expect(grid).toBeVisible()
  // 验证可见方格的悬浮提示包含用量。
  const newest = grid.locator('.token-activity-cell').last()
  await expect(newest).toHaveAttribute('aria-label', /tokens$/)
  await newest.hover()
  await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('tokens')
})
