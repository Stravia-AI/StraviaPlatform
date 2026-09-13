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
    const path = new URL(route.request().url()).pathname
    if (path.endsWith('/auth/state')) {
      await route.fulfill({
        json: { mode: 'server', authenticated: true, setup_authorized: false, username: 'locale-admin' },
      })
      return
    }
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
  await page.route('**/api/v1/auth/state', async (route) => {
    await route.fulfill({ json: { mode: 'server', authenticated: false, setup_authorized: false, username: null } })
  })
  await page.route('**/api/v1/auth/login', async (route) => {
    await route.fulfill({ status: 401, json: { error: 'unauthorized' } })
  })
  await page.goto('/login')
  await page.getByLabel('Username').fill('draft-admin')
  await page.getByLabel('Password', { exact: true }).fill('draft-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await expect(page.getByText('The username or password is incorrect.', { exact: true })).toBeVisible()

  let navigationCount = 0
  page.on('framenavigated', (frame) => {
    if (frame === page.mainFrame()) navigationCount += 1
  })
  await page.getByRole('button', { name: 'Language', exact: true }).click()
  await page.getByRole('option', { name: '简体中文' }).click()

  await expect(page.getByRole('heading', { name: '登录 Stravia' })).toBeVisible()
  await expect(page.getByText('用户名或密码错误。', { exact: true })).toBeVisible()
  await expect(page.locator('[aria-label="语言"]')).toBeVisible()
  await expect(page.getByLabel('用户名')).toHaveValue('draft-admin')
  await expect(page.getByLabel('密码', { exact: true })).toHaveValue('draft-password')
  await expect(page.getByRole('button', { name: '显示密码' })).toBeVisible()
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

test('localized Request Records keep one local timestamp across canvas and detail', async ({ page }) => {
  await prepareLocalePage(page, ['zh-CN'], 'zh-CN')
  const startedAt = Date.UTC(2026, 0, 2, 0, 4, 5)
  const interaction = {
    id: 'localized-interaction',
    root_id: 'localized-root',
    parent_interaction_id: null,
    generation_root_id: null,
    first_route_id: 'gpt-route',
    first_model_display_name: 'GPT 5.6',
    status: 'completed',
    started_at: startedAt,
    last_active_at: startedAt + 42,
    input_preview: '用户输入问题',
    visible_tail: '客户端可见回答',
    usage: {
      input_tokens: 1200,
      output_tokens: 34,
      cache_read_tokens: null,
      cache_write_tokens: null,
      reasoning_tokens: null,
    },
    debug_status: 'none',
    observation_gap: false,
    matched: true,
    last_event_sequence: 4,
  }
  const root = { id: 'localized-root', last_active_at: startedAt + 42, interactions: [interaction] }
  await page.route('**/api/v1/observations/interactions?**', async (route) => {
    await route.fulfill({
      json: {
        data: {
          anchor_at: startedAt + 86_400_000,
          window_index: 0,
          window_start: startedAt,
          window_end: startedAt + 86_400_000,
          roots: [root],
          root_total: 1,
          next_cursor: null,
          snapshot_sequence: 4,
        },
      },
    })
  })
  await page.route('**/api/v1/observations/interactions/localized-interaction**', async (route) => {
    const summary = { interaction, root, snapshot_sequence: 4 }
    const path = new URL(route.request().url()).pathname
    const data = path.endsWith('/events') ? { runs: [], snapshot_sequence: 4, next_cursor: null }
      : path.endsWith('/summary') ? summary : { ...summary, runs: [], older_events_cursor: null }
    await route.fulfill({ json: { data } })
  })
  await page.goto('/logs')

  const localTimestamp = '2026/1/2 08:04:05'
  await expect(page.getByText(localTimestamp, { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'GPT 5.6, 已完成', exact: true }).click()
  const inspector = page.getByRole('complementary', { name: '观测详情' })
  await expect(inspector).toBeVisible()
  await expect(inspector.getByRole('log', { name: '对话' }).locator('time').first()).toHaveText(localTimestamp)
  await expect(inspector.getByRole('button', { name: '关闭' })).toBeVisible()
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
