import { expect, test, type Page } from '@playwright/test'

import { prepareApp } from './prepare-app'

async function stubStats(page: Page): Promise<void> {
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
    const now = Date.now()
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
test('token activity grid granularity follows the range selector', async ({ page }) => {
  await prepareApp(page)
  await stubStats(page)
  await page.setViewportSize({ width: 1280, height: 900 })

  const ranges = [
    { value: '6', label: 'Last 6h', expectedCells: 25 },
    { value: '24', label: 'Last 24h', expectedCells: 25 },
    { value: '72', label: 'Last 3d', expectedCells: 13 },
    { value: '168', label: 'Last 7d', expectedCells: 8 },
  ]
  for (const range of ranges) {
    await page.goto('/stats')
    await page.locator('[data-slot="select-trigger"]').click()
    await page.getByRole('option', { name: range.label }).click()
    const grid = page.getByRole('group', { name: 'Token activity' })
    await expect(grid).toBeVisible()
    await expect(grid.locator('.token-activity-cell')).toHaveCount(range.expectedCells)
  }
})

test('token activity cell tooltip reports the bucket time and token total', async ({ page }) => {
  await prepareApp(page)
  await stubStats(page)
  await page.goto('/stats')

  const grid = page.getByRole('group', { name: 'Token activity' })
  await expect(grid).toBeVisible()
  // 最新格（末位）mock 了 190M 输入 + 其余维度，tooltip 汇总为 198M 级文案。
  const newest = grid.locator('.token-activity-cell').last()
  await expect(newest).toHaveAttribute('aria-label', /tokens$/)
  await newest.hover()
  await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('tokens')
})
