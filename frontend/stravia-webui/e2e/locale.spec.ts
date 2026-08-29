import { expect, test, type Page } from '@playwright/test'

test.use({ timezoneId: 'Asia/Shanghai' })

async function prepareLocalePage(page: Page, languages: string[], savedLocale?: string): Promise<void> {
  await page.addInitScript(
    ({ clientLanguages, locale }) => {
      Object.defineProperty(navigator, 'languages', { configurable: true, value: clientLanguages })
      Object.defineProperty(navigator, 'language', { configurable: true, value: clientLanguages[0] ?? '' })

      if (locale === undefined) localStorage.removeItem('stravia-locale')
      else localStorage.setItem('stravia-locale', locale)
    },
    { clientLanguages: languages, locale: savedLocale },
  )

  await page.route('**/api/v1/**', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })
}

for (const clientLocale of ['zh-CN', 'zh-SG', 'zh-Hans']) {
  test(`first visit selects and persists Simplified Chinese for ${clientLocale}`, async ({ page }) => {
    await prepareLocalePage(page, [clientLocale, 'en-US'])
    await page.goto('/')

    await expect(page.getByRole('heading', { name: '概览', exact: true })).toBeVisible()
    await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')
    await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-locale'))).toBe('zh-CN')
  })
}

for (const clientLocale of ['zh-TW', 'zh-HK', 'zh-Hant']) {
  test(`first visit keeps English for unsupported Traditional Chinese ${clientLocale}`, async ({ page }) => {
    await prepareLocalePage(page, [clientLocale])
    await page.goto('/')

    await expect(page.getByRole('heading', { name: 'Overview', exact: true })).toBeVisible()
    await expect(page.locator('html')).toHaveAttribute('lang', 'en-US')
    await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-locale'))).toBe('en-US')
  })
}

test('saved language takes precedence over the client locale', async ({ page }) => {
  await prepareLocalePage(page, ['zh-CN'], 'en-US')
  await page.goto('/')

  await expect(page.getByRole('heading', { name: 'Overview', exact: true })).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('lang', 'en-US')
  await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-locale'))).toBe('en-US')
})

test('Login switches language without navigation or losing form state', async ({ page }) => {
  await prepareLocalePage(page, ['en-US'])
  await page.route('**/api/v1/status', async (route) => {
    await route.fulfill({ status: 401, json: { error: 'unauthorized' } })
  })
  await page.goto('/login')
  await page.getByLabel('Admin Token').fill('draft-admin-token')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await expect(page.getByText('Invalid token. Check the configured Admin Token.', { exact: true })).toBeVisible()

  let navigationCount = 0
  page.on('framenavigated', (frame) => {
    if (frame === page.mainFrame()) navigationCount += 1
  })
  await page.getByRole('button', { name: 'Language', exact: true }).click()
  await page.getByRole('option', { name: '简体中文' }).click()

  await expect(page.getByRole('heading', { name: '登录 Stravia' })).toBeVisible()
  await expect(page.getByText('Token 无效，请检查已配置的 Admin Token。', { exact: true })).toBeVisible()
  await expect(page.locator('[aria-label="语言"]')).toBeVisible()
  await expect(page.getByLabel('Admin Token')).toHaveValue('draft-admin-token')
  await expect(page.getByRole('button', { name: '显示 Token' })).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')
  await expect(page).toHaveURL(/\/login$/)
  expect(navigationCount).toBe(0)
})

test('Settings uses the shared language selector and updates immediately', async ({ page }) => {
  await prepareLocalePage(page, ['en-US'], 'en-US')
  await page.goto('/settings')

  await expect(page.getByRole('heading', { name: 'Settings', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Language', exact: true }).click()
  await page.getByRole('option', { name: '简体中文' }).click()

  await expect(page.getByRole('heading', { name: '设置', exact: true })).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')
  await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-locale'))).toBe('zh-CN')
})

test('localized Logs keep one local timestamp across list and detail surfaces', async ({ page }) => {
  await prepareLocalePage(page, ['zh-CN'], 'zh-CN')
  const requestLog = {
    id: 'localized-log',
    created_at: Date.UTC(2026, 0, 2, 0, 4, 5),
    method: 'POST',
    path: '/v1/chat/completions',
    client_status_code: 200,
    latency_total_ms: 42,
    input_tokens: 1200,
    output_tokens: 34,
    is_stream: false,
    stream_chunks_count: 0,
  }
  await page.route('**/api/v1/logs**', async (route) => {
    await route.fulfill({ json: { data: { items: [requestLog], total: 1 } } })
  })
  await page.route('**/api/v1/logs/localized-log', async (route) => {
    await route.fulfill({ json: { data: requestLog } })
  })
  await page.goto('/logs')

  const localTimestamp = '2026/1/2 08:04:05'
  await expect(page.getByRole('columnheader', { name: 'Token', exact: true })).toBeVisible()
  await expect(page.getByRole('cell', { name: localTimestamp })).toBeVisible()
  await page.getByRole('button', { name: '查看详情' }).click()

  const detail = page.getByRole('dialog', { name: '请求详情' })
  await expect(detail).toBeVisible()
  await expect(detail.getByText(localTimestamp, { exact: true })).toBeVisible()
  await expect(detail.getByRole('button', { name: '关闭请求详情' })).toBeVisible()
})

test('Logs keep Token metrics inside their column at desktop width', async ({ page }) => {
  await page.setViewportSize({ width: 1254, height: 784 })
  await prepareLocalePage(page, ['zh-CN'], 'zh-CN')
  const requestLog = {
    id: 'token-layout-log',
    created_at: Date.UTC(2026, 7, 28, 8, 18, 24),
    method: 'POST',
    path: '/v1/responses',
    client_status_code: 200,
    latency_total_ms: 2600,
    stream_first_chunk_ms: 1900,
    latency_upstream_ms: 2300,
    input_tokens: 10361,
    output_tokens: 128,
    cache_read_tokens: 10280,
    cache_write_tokens: 0,
    is_stream: true,
    stream_chunks_count: 20,
    model_name: 'grok-4.6',
    upstream_model: 'grok-4.6',
    provider_name: 'Grok',
  }
  const requestLogs = [
    requestLog,
    {
      ...requestLog,
      id: 'token-layout-log-codex',
      model_name: 'gpt-5.6-luna',
      upstream_model: 'gpt-5.6-luna',
      provider_name: 'Codex',
      thinking_level: 'medium',
    },
    {
      ...requestLog,
      id: 'token-layout-log-sol',
      model_name: 'gpt-5.6-sol',
      upstream_model: 'gpt-5.6-sol',
      provider_name: 'Codex',
      thinking_level: 'medium',
    },
  ]
  await page.route('**/api/v1/logs**', async (route) => {
    await route.fulfill({ json: { data: { items: requestLogs, total: requestLogs.length } } })
  })
  await page.goto('/logs')

  await expect(page.getByRole('columnheader', { name: '请求' })).toHaveCount(0)
  const tokenCell = page.locator('tbody td').filter({ hasText: 'C-IN' }).first()
  await expect(tokenCell).toBeVisible()
  expect(await tokenCell.evaluate((cell) => cell.scrollWidth)).toBeLessThanOrEqual(
    await tokenCell.evaluate((cell) => cell.clientWidth),
  )
  await expect(page.getByRole('button', { name: '查看详情' }).first()).toBeInViewport()
  await page.getByRole('button', { name: '列' }).click()
  const requestColumnToggle = page.getByRole('menuitemcheckbox', { name: '请求' })
  await expect(requestColumnToggle).not.toBeChecked()
  await requestColumnToggle.click()
  await expect(page.getByRole('columnheader', { name: '请求' })).toBeVisible()

  await page.setViewportSize({ width: 1063, height: 800 })
  await expect(page.getByRole('table', { name: '最近请求' })).toBeHidden()
  await expect(page.locator('.route-mobile-list').filter({ hasText: 'C-IN' })).toBeVisible()
})

test('known backend errors localize while unknown diagnostics remain visible', async ({ page }) => {
  await prepareLocalePage(page, ['zh-CN'], 'zh-CN')
  let error = JSON.stringify({ code: 'AUTH_SESSION_REPLACED', message: 'internal replacement diagnostic' })
  await page.route('**/api/v1/stats/overview**', async (route) => {
    await route.fulfill({ status: 400, json: { error } })
  })
  await page.goto('/')

  await expect(page.getByText('此次 OAuth 登录已被新的登录取代，请继续完成最新的登录。')).toBeVisible()
  await expect(page.getByText('internal replacement diagnostic')).toHaveCount(0)

  error = 'upstream exploded'
  await page.getByRole('button', { name: '重试' }).click()
  await expect(page.getByText('请求失败：upstream exploded')).toBeVisible()
})
