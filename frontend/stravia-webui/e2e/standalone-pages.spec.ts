import { expect, test, type Page, type Route } from '@playwright/test'

import { prepareApp } from './prepare-app'

// 独立页面（登录、初始化、404、全局错误、Server／Desktop 启动）：不显示应用导航、不使用内容容器，
// 各自限宽居中；失败原因、恢复动作与真实进度保持可见。

const viewports = [
  { width: 2560, height: 1440 },
  { width: 1440, height: 900 },
  { width: 390, height: 844 },
  { width: 320, height: 640 },
] as const

type Box = { x: number; y: number; width: number; height: number }

async function box(page: Page, selector: string): Promise<Box> {
  const found = await page.locator(selector).first().boundingBox()
  if (!found) throw new Error(`No layout box for ${selector}`)
  return found
}

function rem(value: number): number {
  return value * 16
}

/** 所有可见元素都落在视口内，且滚动容器没有横向溢出。 */
async function expectContainedHorizontally(page: Page): Promise<void> {
  const offenders = await page.evaluate(() => {
    const limit = window.innerWidth + 0.5
    const root = document.documentElement
    const result: string[] = []
    if (root.scrollWidth > window.innerWidth) result.push(`document scrollWidth ${root.scrollWidth}`)
    const main = document.querySelector('main')
    for (let node = main?.parentElement; node && node !== root; node = node.parentElement) {
      if (node.scrollWidth > node.clientWidth) result.push(`${node.tagName} scrollWidth ${node.scrollWidth}`)
    }
    for (const element of Array.from(document.querySelectorAll('main *'))) {
      if (element.closest('[data-slot="progress"], .sr-only, [aria-hidden="true"]')) continue
      const rect = element.getBoundingClientRect()
      if (rect.width <= 1 || rect.height <= 1) continue
      if (rect.left < -0.5 || rect.right > limit) result.push(`${element.tagName} ${rect.left}..${rect.right}`)
    }
    return result
  })
  expect(offenders).toEqual([])
}

async function expectNoApplicationNavigation(page: Page): Promise<void> {
  await expect(page.locator('a[href="/providers"]')).toHaveCount(0)
  await expect(page.locator('[data-slot="sidebar"]')).toHaveCount(0)
}

async function chooseLanguage(page: Page, name: string): Promise<void> {
  await page.getByRole('button', { name: /^(Language|语言)$/ }).click()
  await page.getByRole('option', { name }).click()
}

async function mockServer(page: Page, auth: Record<string, unknown>): Promise<void> {
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'en-US'))
  await page.route('**/api/v1/**', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path.endsWith('/startup')) return route.fulfill({ json: { status: 'ready', progress: null } })
    if (path.endsWith('/auth/state')) return route.fulfill({ json: auth })
    return route.fulfill({ json: { data: [] } })
  })
}

const signedOut = { mode: 'server', authenticated: false, setup_authorized: false, username: null }

test.describe('Login', () => {
  test('is a standalone task page with one heading and the form on the first screen', async ({ page }) => {
    await mockServer(page, signedOut)
    for (const viewport of viewports) {
      await page.setViewportSize(viewport)
      await page.goto('/login')

      await expect(page.getByRole('heading', { level: 1 })).toHaveCount(1)
      await expect(page.getByRole('heading', { level: 1, name: 'Sign in to Stravia' })).toBeVisible()
      await expect(page.getByRole('heading', { level: 2 })).toHaveCount(0)
      await expectNoApplicationNavigation(page)

      const username = await box(page, '#admin-username')
      const submit = await box(page, 'button[type="submit"]')
      expect(username.y + username.height).toBeLessThanOrEqual(viewport.height)
      expect(submit.y + submit.height).toBeLessThanOrEqual(viewport.height)

      const form = await box(page, 'form')
      expect(form.width).toBeLessThanOrEqual(rem(64))
      await expectContainedHorizontally(page)
    }
  })

  test('drops the duplicated marketing, instruction and language copy but keeps labels and errors', async ({
    page,
  }) => {
    await mockServer(page, signedOut)
    await page.route('**/api/v1/auth/login', (route) => route.fulfill({ status: 503, json: { error: 'down' } }))
    await page.goto('/login')

    await expect(page.getByText('The support platform for AI agents')).toHaveCount(0)
    await expect(page.getByText(/Connect providers, models, and tools/)).toHaveCount(0)
    await expect(page.getByText(/Enter the administrator username and password/)).toHaveCount(0)
    await expect(page.getByText('Choose the language used by Stravia.')).toHaveCount(0)
    await expect(page.getByLabel('Username')).toBeVisible()
    await expect(page.getByLabel('Password', { exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Show password' })).toBeVisible()

    await page.getByLabel('Username').fill('admin')
    await page.getByLabel('Password', { exact: true }).fill('secret')
    await page.keyboard.press('Enter')
    await expect(
      page.getByText('Stravia Server is unavailable. Check the connection and try again.', { exact: true }),
    ).toBeVisible()
    await expect(page.getByLabel('Username')).toBeFocused()
    await expect(page.getByLabel('Password', { exact: true })).toHaveValue('secret')
  })
})

test.describe('Setup', () => {
  const claimable = { mode: 'setup', authenticated: false, setup_authorized: false, username: null }
  const authorized = { mode: 'setup', authenticated: false, setup_authorized: true, username: null }

  test('keeps the title clear of a compact language control at every width', async ({ page }) => {
    await mockServer(page, claimable)
    for (const viewport of viewports) {
      await page.setViewportSize(viewport)
      await page.goto('/setup')
      const heading = page.getByRole('heading', { level: 1, name: 'Set up Stravia Server' })
      await expect(heading).toBeVisible()
      await expect(page.getByRole('heading', { level: 1 })).toHaveCount(1)
      await expectNoApplicationNavigation(page)

      const title = await box(page, 'h1')
      const language = await page.getByRole('button', { name: 'Language', exact: true }).boundingBox()
      expect(language).not.toBeNull()
      expect(language!.width).toBeLessThanOrEqual(rem(12))
      const separate =
        language!.y >= title.y + title.height - 1 ||
        title.y >= language!.y + language!.height - 1 ||
        language!.x >= title.x + title.width - 1 ||
        title.x >= language!.x + language!.width - 1
      expect(separate).toBe(true)
      // 标题在桌面保持单行，在手机上使用整行宽度。
      if (viewport.width >= 1440) expect(title.height).toBeLessThan(45)
      else expect(title.width).toBeGreaterThanOrEqual(viewport.width - 80)
      expect(title.x + title.width).toBeLessThanOrEqual(viewport.width)
      expect((await box(page, 'main > div')).width).toBeLessThanOrEqual(rem(48))
      await expectContainedHorizontally(page)
    }
  })

  test('consumes the one-time token and keeps the draft across a language switch', async ({ page }) => {
    let consumed = false
    const claims: string[] = []
    await mockServer(page, claimable)
    await page.route('**/api/v1/auth/state', (route) =>
      route.fulfill({ json: consumed ? { ...authorized, setup_authorized: false } : claimable }),
    )
    await page.route('**/api/v1/setup/claim', async (route) => {
      claims.push(route.request().postDataJSON().token)
      if (consumed || claims[0] !== 'one-time-token') {
        return route.fulfill({ status: 401, json: { code: 'invalid_setup_token', error: 'invalid' } })
      }
      consumed = true
      await route.fulfill({ status: 204 })
    })

    await page.goto('/setup')
    await page.locator('#setup-token').fill('one-time-token')
    let navigations = 0
    page.on('framenavigated', (frame) => {
      if (frame === page.mainFrame()) navigations += 1
    })
    await chooseLanguage(page, '简体中文')
    await expect(page.getByRole('heading', { level: 1, name: '设置 Stravia Server' })).toBeVisible()
    await expect(page.locator('#setup-token')).toHaveValue('one-time-token')
    expect(navigations).toBe(0)
    await chooseLanguage(page, 'English')

    await page.getByRole('button', { name: 'Continue', exact: true }).click()
    await expect(page.locator('#setup-token')).toHaveCount(0)
    await expect(page.getByRole('heading', { level: 2, name: 'Database' })).toBeVisible()
    expect(claims).toEqual(['one-time-token'])

    // 令牌已被消费：刷新后回到认领步骤，字段为空，再次提交同一令牌会被拒绝并保留输入。
    await page.reload()
    await expect(page.locator('#setup-token')).toHaveValue('')
    await page.locator('#setup-token').fill('one-time-token')
    await page.getByRole('button', { name: 'Continue', exact: true }).click()
    await expect(
      page.getByText(
        'This setup token is invalid or has already been used. Use the token from the current Server process.',
        { exact: true },
      ),
    ).toBeVisible()
    await expect(page.locator('#setup-token')).toHaveValue('one-time-token')
    await expect(page.getByRole('heading', { level: 2, name: 'Database' })).toHaveCount(0)
    expect(claims).toEqual(['one-time-token', 'one-time-token'])
  })

  test('submits database, pool and administrator values as one draft after a fresh connection test', async ({
    page,
  }) => {
    const tests: unknown[] = []
    let completion: Record<string, unknown> | undefined
    await mockServer(page, authorized)
    await page.route('**/api/v1/setup/test', async (route) => {
      tests.push(route.request().postDataJSON().database)
      await route.fulfill({ status: 204 })
    })
    await page.route('**/api/v1/setup/complete', async (route) => {
      completion = route.request().postDataJSON()
      await route.fulfill({ json: { mode: 'server' } })
    })

    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/setup')
    await expect(page.getByRole('heading', { level: 2, name: 'Database' })).toBeVisible()

    // 数据库与管理员是相隔 32px 的命名区段；卡片外框不再出现。
    const database = await box(page, 'section[aria-labelledby]:has(#database-backend)')
    const administrator = await box(page, 'section[aria-labelledby]:has(#setup-username)')
    expect(Math.round(administrator.y - (database.y + database.height))).toBe(32)
    await expect(page.locator('[data-slot="card"]')).toHaveCount(0)
    await expect(page.getByText(/Create the single administrator account/)).toHaveCount(0)

    // 完成按钮因未测试而禁用时就地说明原因。
    const complete = page.getByRole('button', { name: 'Complete setup', exact: true })
    const reason = page.getByText('Test the database connection before completing setup.', { exact: true })
    await expect(complete).toBeDisabled()
    await expect(reason).toBeVisible()

    // SQLite 没有连接池；PostgreSQL 的 URL 与测试动作常驻，连接池收进 Advanced。
    await expect(page.locator('#postgres-url')).toHaveCount(0)
    await page.locator('#database-backend').click()
    await page.getByRole('option', { name: 'PostgreSQL' }).click()
    await expect(page.locator('#postgres-url')).toBeVisible()
    const advanced = page.getByRole('button', { name: 'Advanced', exact: true })
    await expect(advanced).toHaveAttribute('aria-expanded', 'false')
    await expect(page.locator('#max-connections')).not.toBeVisible()

    await page.locator('#postgres-url').fill('postgresql://stravia:pw@db.internal/stravia')
    await page.locator('#setup-client-base-url').fill('https://stravia.example/base')
    await page.locator('#setup-username').fill('administrator')
    await page.locator('#setup-password').fill('a-secure-setup-password')
    await page.locator('#setup-confirm-password').fill('a-secure-setup-password')
    await page.getByRole('button', { name: 'Test connection', exact: true }).click()
    await expect(page.getByText('Database connection succeeded. You can complete setup.')).toBeVisible()
    await expect(complete).toBeEnabled()
    await expect(reason).toHaveCount(0)

    // 连接池变更使测试失效，必须重新测试。
    await advanced.click()
    await expect(advanced).toHaveAttribute('aria-expanded', 'true')
    await page.locator('#max-connections').fill('12')
    await expect(complete).toBeDisabled()
    await expect(reason).toBeVisible()
    await expect(page.getByText('Database connection succeeded. You can complete setup.')).toHaveCount(0)

    // 非正整数的连接池值会在 Advanced 折叠后仍可见地阻止测试。
    await page.locator('#min-connections').fill('0')
    const invalid = page.getByText('Connection pool values must be positive whole numbers.', { exact: true })
    await expect(invalid).toBeVisible()
    await expect(page.getByRole('button', { name: 'Test connection', exact: true })).toBeDisabled()
    await advanced.click()
    await expect(invalid).toBeVisible()
    await advanced.click()
    await page.locator('#min-connections').fill('2')
    await expect(invalid).toHaveCount(0)

    await page.getByRole('button', { name: 'Test connection', exact: true }).click()
    await expect(complete).toBeEnabled()

    // 数据库类型变更同样使测试失效；回到 PostgreSQL 后保留的值需重新测试。
    await page.locator('#database-backend').click()
    await page.getByRole('option', { name: 'SQLite' }).click()
    await expect(complete).toBeDisabled()
    await page.locator('#database-backend').click()
    await page.getByRole('option', { name: 'PostgreSQL' }).click()
    await expect(complete).toBeDisabled()
    await page.getByRole('button', { name: 'Test connection', exact: true }).click()
    await expect(complete).toBeEnabled()
    expect(completion).toBeUndefined()

    await complete.click()
    await expect(page).toHaveURL(/\/login$/)
    const url = 'postgresql://stravia:pw@db.internal/stravia'
    const pooled = { backend: 'postgres', url, max_connections: 12, min_connections: 2 }
    expect(tests).toEqual([{ backend: 'postgres', url }, pooled, pooled])
    expect(completion).toEqual({
      database: {
        backend: 'postgres',
        url: 'postgresql://stravia:pw@db.internal/stravia',
        max_connections: 12,
        min_connections: 2,
      },
      username: 'administrator',
      password: 'a-secure-setup-password',
      client_base_url: 'https://stravia.example/base',
    })
  })

  test('explains the permanent data flow and reports test and completion failures where they happen', async ({
    page,
  }) => {
    await mockServer(page, authorized)
    await page.route('**/api/v1/setup/test', (route) =>
      route.fulfill({ status: 400, json: { code: 'database_unavailable', error: 'down' } }),
    )
    await page.goto('/setup')

    await expect(page.getByText(/Enter the complete URL, including protocol, port and path prefix/)).toBeVisible()
    await page.getByRole('button', { name: 'Test connection', exact: true }).click()
    await expect(
      page.getByText('The database connection failed. Check the address, credentials and database availability.'),
    ).toBeVisible()
    await expect(page.getByRole('button', { name: 'Complete setup', exact: true })).toBeDisabled()
    // 数据库说明属于可选解释，收进问号提示。
    const help = page.getByRole('button', { name: 'More about this field' }).first()
    await help.focus()
    await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Choose SQLite')
  })
})

test.describe('Not found', () => {
  test('keeps the status beside the heading and wraps an extreme requested path', async ({ page }) => {
    await prepareApp(page)
    const longPath = `/${'unmoved-segment'.repeat(30)}`
    for (const viewport of viewports) {
      await page.setViewportSize(viewport)
      await page.goto(longPath)

      const heading = page.getByRole('heading', { level: 1, name: 'Page not found' })
      await expect(heading).toBeVisible()
      await expect(page.getByRole('heading', { level: 1 })).toHaveCount(1)
      await expect(page.getByText('404 · Stravia')).toHaveCount(0)
      await expectNoApplicationNavigation(page)

      const title = await heading.boundingBox()
      const status = await page.getByText('404', { exact: true }).boundingBox()
      expect(title && status).toBeTruthy()
      expect(status!.x + status!.width).toBeLessThanOrEqual(title!.x + 1)
      expect(title!.x - (status!.x + status!.width)).toBeLessThanOrEqual(24)
      expect(Math.abs(status!.y + status!.height / 2 - (title!.y + title!.height / 2))).toBeLessThanOrEqual(
        title!.height,
      )

      await expect(page.getByText(longPath, { exact: false })).toBeVisible()
      const section = await box(page, 'section[aria-labelledby]')
      expect(section.width).toBeLessThanOrEqual(rem(42))
      await expectContainedHorizontally(page)
      await expect(page.getByRole('link', { name: 'Go to overview' })).toHaveAttribute('href', '/')
    }
  })
})

test.describe('Global error', () => {
  // 路由代码块由浏览器在客户端导航时按需加载；读取它自己的 resource timing，
  // 不猜文件名，也不依赖 networkidle。
  async function statsNodes(page: Page): Promise<string[]> {
    const scriptUrls = () =>
      page.evaluate(() =>
        performance
          .getEntriesByType('resource')
          .map((entry) => entry.name)
          .filter((name) => /\/nodes\/[^/]+\.js$/.test(new URL(name).pathname)),
      )
    await prepareApp(page)
    await page.goto('/')
    await expect(page.getByRole('heading', { level: 1, name: 'New conversation', exact: true })).toBeVisible()
    const before = new Set(await scriptUrls())
    await page
      .getByRole('navigation', { name: 'Primary navigation' })
      .getByRole('link', { name: 'Usage', exact: true })
      .click()
    await expect(page.getByRole('heading', { level: 1, name: 'Usage statistics', exact: true })).toBeVisible()
    const nodes = (await scriptUrls()).filter((url) => !before.has(url))
    expect(nodes.length).toBeGreaterThan(0)
    return nodes
  }

  async function blockStatsNodes(page: Page, handler: (route: Route) => unknown): Promise<string[]> {
    const nodes = await statsNodes(page)
    for (const node of nodes) await page.route(node, handler)
    return nodes
  }

  test('shows a localized safe reason beside the status, and reload and home recover', async ({ page }) => {
    const nodes = await blockStatsNodes(page, (route) => route.abort())
    await page.setViewportSize({ width: 390, height: 844 })
    await page.goto('/stats')

    const heading = page.getByRole('heading', { level: 1, name: 'This page is temporarily unavailable' })
    await expect(heading).toBeVisible()
    await expect(page.getByRole('heading', { level: 1 })).toHaveCount(1)
    await expectNoApplicationNavigation(page)
    const title = await heading.boundingBox()
    const status = await page.getByText('500', { exact: true }).boundingBox()
    expect(status!.x + status!.width).toBeLessThanOrEqual(title!.x + 1)
    await expect(page.getByText('Error · 500')).toHaveCount(0)

    await expect(page.getByText(/part of this page could not be loaded/i)).toBeVisible()
    await expect(
      page.getByText('Reload the page. If the problem continues, check that Stravia is running and try again.'),
    ).toBeVisible()
    await expect(page.locator('body')).not.toContainText('dynamically imported module')
    for (const node of nodes) await expect(page.locator('body')).not.toContainText(node)
    await expectContainedHorizontally(page)

    await page.setViewportSize({ width: 1440, height: 900 })
    expect((await box(page, 'section[aria-labelledby], main > section')).width).toBeLessThanOrEqual(rem(42))

    const [reloaded] = await Promise.all([
      page.waitForEvent('load'),
      page.getByRole('button', { name: 'Reload page', exact: true }).click(),
    ])
    expect(reloaded.url()).toContain('/stats')
    await expect(heading).toBeVisible()

    for (const node of nodes) await page.unroute(node)
    await page.getByRole('link', { name: 'Go to overview' }).click()
    await expect(page).toHaveURL(/\/$/)
    await expect(page.getByRole('heading', { name: 'New conversation', exact: true })).toBeVisible()
  })

  test('falls back to the generic recovery text without echoing sensitive exception details', async ({ page }) => {
    await blockStatsNodes(page, (route) =>
      route.fulfill({
        contentType: 'text/javascript',
        body: "throw new Error('connect ECONNREFUSED postgres://admin:hunter2@10.0.0.5:5432/stravia')",
      }),
    )
    await page.goto('/stats')

    await expect(page.getByRole('heading', { level: 1, name: 'This page is temporarily unavailable' })).toBeVisible()
    await expect(
      page.getByText('Reload the page. If the problem continues, check that Stravia is running and try again.'),
    ).toBeVisible()
    await expect(page.getByText(/part of this page could not be loaded/i)).toHaveCount(0)
    await expect(page.locator('body')).not.toContainText('hunter2')
    await expect(page.locator('body')).not.toContainText('10.0.0.5')
    await expect(page.locator('body')).not.toContainText('ECONNREFUSED')
    await expect(page.getByRole('button', { name: 'Reload page', exact: true })).toBeVisible()
    await expect(page.getByRole('link', { name: 'Go to overview' })).toHaveAttribute('href', '/')
  })
})

test.describe('Server startup', () => {
  async function mockStartup(page: Page, startup: { status: number; json?: unknown }): Promise<void> {
    await page.addInitScript(() => localStorage.setItem('stravia-locale', 'en-US'))
    await page.route('**/api/v1/startup', (route) => route.fulfill(startup))
    // 订阅保持挂起：不伪造就绪，也不让连接错误掩盖真实进度。
    await page.route('**/api/v1/startup/events', () => {})
    await page.route('**/api/v1/auth/state', (route) => route.fulfill({ json: signedOut }))
  }

  test('shows the real operation with count and percentage when the total is known', async ({ page }) => {
    await mockStartup(page, {
      status: 200,
      json: {
        status: 'starting',
        progress: { phase: 'history_data', label: 'Migrating history records', completed: 17, total: 42 },
      },
    })
    for (const viewport of viewports) {
      await page.setViewportSize(viewport)
      await page.goto('/')
      await expect(page.getByRole('heading', { level: 1, name: 'Starting Stravia' })).toBeVisible()
      await expect(page.getByText('Migrating history records', { exact: true })).toBeVisible()
      await expect(page.getByText('17 / 42')).toBeVisible()
      await expect(page.getByText('40%')).toBeVisible()
      await expect(page.getByRole('progressbar')).toHaveAttribute('aria-valuenow', /^40(\.\d+)?$/)
      await expect(page.getByText('Current phase')).toHaveCount(0)
      await expect(page.getByText(/Preparing services/)).toHaveCount(0)
      await expectNoApplicationNavigation(page)
      expect((await box(page, 'section[aria-labelledby]')).width).toBeLessThanOrEqual(rem(36))
      await expectContainedHorizontally(page)
    }
  })

  test('shows an indeterminate bar and no invented numbers when the total is unknown', async ({ page }) => {
    await mockStartup(page, {
      status: 200,
      json: {
        status: 'starting',
        progress: { phase: 'schema_migrations', label: 'Applying database updates', completed: 3, total: null },
      },
    })
    await page.goto('/')
    await expect(page.getByText('Applying database updates', { exact: true })).toBeVisible()
    await expect(page.getByRole('progressbar')).not.toHaveAttribute('aria-valuenow', /.+/)
    await expect(page.getByText('%')).toHaveCount(0)
    await expect(page.getByText(/\d+ \/ \d+/)).toHaveCount(0)
    await expect(page).not.toHaveURL(/\/login/)
  })

  test('keeps a startup failure and a connection failure distinct, each with its own recovery', async ({ page }) => {
    await mockStartup(page, { status: 200, json: { status: 'failed', progress: null } })
    await page.goto('/')
    await expect(page.getByRole('heading', { level: 1, name: 'Server startup failed' })).toBeVisible()
    await expect(page.getByText(/Check the server logs, resolve the startup problem/)).toBeVisible()
    const refresh = page.getByRole('button', { name: 'Refresh and reconnect' })
    await expect(refresh).toBeVisible()
    await expect(page).not.toHaveURL(/\/login/)
    // 恢复动作按内容宽度排布，不拉成整列条带。
    expect((await refresh.boundingBox())!.width).toBeLessThan(rem(24))
    const [reloaded] = await Promise.all([page.waitForEvent('load'), refresh.click()])
    expect(new URL(reloaded.url()).pathname).toBe('/')

    await page.unroute('**/api/v1/startup')
    await page.route('**/api/v1/startup', (route) => route.fulfill({ status: 503, body: '' }))
    await page.goto('/')
    await expect(page.getByRole('heading', { level: 1, name: 'Unable to check server startup' })).toBeVisible()
    await expect(page.getByText(/Check your connection and whether the service is running/)).toBeVisible()
    await expect(page.getByText(/Check the server logs/)).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Refresh and reconnect' })).toBeVisible()
    await expect(page).not.toHaveURL(/\/login/)
  })
})

interface DesktopOptions {
  state: Record<string, unknown>
  failing?: string[]
  clipboard?: 'ok' | 'denied'
}

const desktopFailure = {
  status: 'failed',
  stage: 'gateway',
  progress: null,
  error: { code: 'gateway', details: 'port 23471 already in use' },
  warnings: [],
  logPath: '/home/user/.local/share/stravia/logs/startup.log',
  previousFailure: false,
}

async function installDesktop(page: Page, options: DesktopOptions): Promise<void> {
  await page.addInitScript(
    (init) => {
      const callbacks = new Map<number, (event: unknown) => void>()
      const listeners: ((event: unknown) => void)[] = []
      const calls: string[] = []
      let nextId = 1
      const host = window as unknown as Record<string, unknown>
      host.__desktopCalls = calls
      host.__desktopState = init.state
      host.__emitDesktopState = (payload: unknown) => {
        host.__desktopState = payload
        for (const listener of listeners) listener({ event: 'desktop-startup-changed', id: 1, payload })
      }
      host.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} }
      host.__TAURI_INTERNALS__ = {
        metadata: { currentWindow: { label: 'main' }, currentWebview: { windowLabel: 'main', label: 'main' } },
        transformCallback(callback: (event: unknown) => void) {
          const id = nextId++
          callbacks.set(id, callback)
          return id
        },
        unregisterCallback(id: number) {
          callbacks.delete(id)
        },
        async invoke(command: string, args?: { event?: string; handler?: number }) {
          await Promise.resolve()
          calls.push(command)
          if (command === 'plugin:event|listen') {
            const handler = callbacks.get(args?.handler ?? -1)
            if (handler && args?.event === 'desktop-startup-changed') listeners.push(handler)
            return args?.handler
          }
          if (command === 'plugin:event|unlisten') return undefined
          if (command === 'get_desktop_startup_state') return host.__desktopState
          if (command === 'get_server_port') return Number(location.port)
          if (command === 'get_admin_session') return { access_token: 'desktop-session' }
          if (init.failing.includes(command)) throw new Error('native command failed')
          return undefined
        },
      }
      if (init.clipboard === 'denied') {
        Object.defineProperty(navigator, 'clipboard', {
          configurable: true,
          value: { writeText: () => Promise.reject(new DOMException('denied', 'NotAllowedError')) },
        })
      }
    },
    { state: options.state, failing: options.failing ?? [], clipboard: options.clipboard ?? 'ok' },
  )
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'en-US'))
}

function desktopCalls(page: Page): Promise<string[]> {
  return page.evaluate(() => [...((window as unknown as { __desktopCalls: string[] }).__desktopCalls ?? [])])
}

test.describe('Desktop startup', () => {
  test('shows only the real operation and progress while starting', async ({ page }) => {
    await installDesktop(page, {
      state: {
        ...desktopFailure,
        status: 'starting',
        error: null,
        logPath: null,
        progress: { phase: 'history_data', label: 'Migrating history records', completed: 5, total: 20 },
      },
    })
    for (const viewport of viewports) {
      await page.setViewportSize(viewport)
      await page.goto('/')
      await expect(page.getByRole('heading', { level: 1, name: 'Starting Stravia' })).toBeVisible()
      await expect(page.getByText('Migrating history records', { exact: true })).toBeVisible()
      await expect(page.getByText('5 / 20')).toBeVisible()
      await expect(page.getByText('25%')).toBeVisible()
      await expect(page.getByText('Desktop startup', { exact: true })).toHaveCount(0)
      await expect(page.getByText('Current phase')).toHaveCount(0)
      await expect(page.getByText(/Preparing local services/)).toHaveCount(0)
      await expectNoApplicationNavigation(page)
      expect((await box(page, 'section[aria-labelledby]')).width).toBeLessThanOrEqual(rem(42))
      await expectContainedHorizontally(page)
    }
  })

  test('keeps failure stage, impact, recovery and data guarantee visible without relying on storage or HTTP', async ({
    page,
  }) => {
    await installDesktop(page, { state: desktopFailure })
    const requests: string[] = []
    page.on('request', (request) => {
      if (request.url().includes('/api/v1/')) requests.push(request.url())
    })
    for (const viewport of viewports) {
      await page.setViewportSize(viewport)
      await page.goto('/')
      await expect(page.getByRole('heading', { level: 1, name: 'Stravia could not start' })).toBeVisible()
      await expect(page.getByText('Local gateway', { exact: true }).first()).toBeVisible()
      await expect(page.getByText(/clients and the management interface are unavailable/)).toBeVisible()
      await expect(page.getByText(/Restarting does not reset or repair your data/)).toBeVisible()
      await expect(page.getByTestId('startup-restart')).toBeEnabled()
      await expect(page.getByTestId('startup-exit')).toBeEnabled()
      await expectNoApplicationNavigation(page)
      expect((await box(page, 'section[aria-labelledby]')).width).toBeLessThanOrEqual(rem(42))
      await expectContainedHorizontally(page)
      // 最后的恢复动作可达。
      await page.getByTestId('startup-restart').scrollIntoViewIfNeeded()
      const restart = await box(page, '[data-testid="startup-restart"]')
      expect(restart.y + restart.height).toBeLessThanOrEqual(viewport.height)
    }
    expect(requests).toEqual([])
  })

  test('expands exact diagnostics on demand and reports copy results', async ({ page, context }) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write'])
    await installDesktop(page, { state: desktopFailure })
    await page.goto('/')

    await expect(page.getByText('port 23471 already in use')).not.toBeVisible()
    await page.getByText('Technical details', { exact: true }).click()
    await expect(page.getByText('port 23471 already in use')).toBeVisible()
    await expect(page.getByText(/Startup log: \/home\/user\/\.local\/share\/stravia\/logs\/startup\.log/)).toBeVisible()
    await page.getByTestId('startup-copy').click()
    await expect(page.getByText('Diagnostics copied to the clipboard.')).toBeVisible()
    const copied = await page.evaluate(() => navigator.clipboard.readText())
    expect(copied).toContain('Error code: gateway')
    expect(copied).toContain('Details: port 23471 already in use')
    await expectContainedHorizontally(page)
  })

  test('reports a copy that the browser denies', async ({ page }) => {
    await installDesktop(page, { state: desktopFailure, clipboard: 'denied' })
    await page.goto('/')
    await page.getByText('Technical details', { exact: true }).click()
    await page.getByTestId('startup-copy').click()
    await expect(page.getByText(/Could not copy the diagnostics/)).toBeVisible()
  })

  test('surfaces failed restart, exit and log actions and re-enables the buttons', async ({ page }) => {
    await installDesktop(page, {
      state: desktopFailure,
      failing: ['restart_desktop', 'exit_desktop', 'open_desktop_logs'],
    })
    await page.goto('/')

    await page.getByTestId('startup-restart').click()
    await expect(page.getByRole('alert').filter({ hasText: 'Stravia could not restart.' })).toBeVisible()
    await expect(page.getByTestId('startup-restart')).toBeEnabled()
    await page.getByTestId('startup-exit').click()
    await expect(page.getByRole('alert').filter({ hasText: 'Stravia could not exit.' })).toBeVisible()
    await page.getByText('Technical details', { exact: true }).click()
    await page.getByTestId('startup-logs').click()
    await expect(page.getByRole('alert').filter({ hasText: 'The startup log could not be opened.' })).toBeVisible()
    expect(await desktopCalls(page)).toEqual(
      expect.arrayContaining(['restart_desktop', 'exit_desktop', 'open_desktop_logs']),
    )
  })

  test('offers the log entry only when a log exists and shows a silent-start failure like any other', async ({
    page,
  }) => {
    await installDesktop(page, {
      state: {
        ...desktopFailure,
        stage: 'desktop',
        error: { code: 'desktop', details: 'silent start could not create the tray' },
        logPath: null,
      },
    })
    await page.goto('/')
    await expect(page.getByRole('heading', { level: 1, name: 'Stravia could not start' })).toBeVisible()
    await expect(page.getByText(/could not complete startup/i)).toBeVisible()
    await page.getByText('Technical details', { exact: true }).click()
    await expect(page.getByText('silent start could not create the tray')).toBeVisible()
    await expect(page.getByTestId('startup-logs')).toHaveCount(0)
    await expect(page.getByTestId('startup-copy')).toBeVisible()
  })

  test('closing the failed recovery window exits instead of continuing to the application', async ({ page }) => {
    await installDesktop(page, { state: desktopFailure })
    await page.goto('/')
    await page.getByRole('button', { name: 'Close window' }).click()
    await expect.poll(() => desktopCalls(page)).toContain('exit_desktop')
  })

  test('shows optional integration warnings only once the gateway is ready and never as a gateway failure', async ({
    page,
  }) => {
    await prepareApp(page)
    await page.route('**/api/v1/auth/state', (route) =>
      route.fulfill({ json: { mode: 'desktop', authenticated: true, setup_authorized: false, username: null } }),
    )
    await installDesktop(page, {
      state: {
        status: 'ready',
        stage: 'desktop',
        progress: null,
        error: null,
        warnings: [{ code: 'tray', details: 'no status notifier host' }],
        logPath: '/home/user/.local/share/stravia/logs/startup.log',
        previousFailure: true,
      },
      failing: ['open_desktop_logs'],
    })
    await page.goto('/')

    await expect(page.getByText('A desktop integration is unavailable')).toBeVisible()
    await expect(page.getByText(/system tray integration is unavailable/)).toBeVisible()
    await expect(page.getByText('Stravia recovered from an earlier interrupted startup')).toBeVisible()
    await expect(page.getByText(/local gateway could not start/i)).toHaveCount(0)
    await expect(page.getByRole('heading', { name: 'Stravia could not start' })).toHaveCount(0)

    await page.getByRole('button', { name: 'Open startup log' }).click()
    await expect(page.getByRole('alert').filter({ hasText: 'The startup log could not be opened.' })).toBeVisible()
    await page.getByRole('button', { name: 'Dismiss startup notice' }).first().click()
    await expect(page.getByText('Stravia recovered from an earlier interrupted startup')).toHaveCount(0)
    await expect(page.getByText('A desktop integration is unavailable')).toBeVisible()
    await page.getByRole('button', { name: 'Dismiss startup notice' }).click()
    await expect(page.getByText('A desktop integration is unavailable')).toHaveCount(0)
  })
})
