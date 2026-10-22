import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('current navigation stays highlighted without hover across routes and sidebar modes', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 })
  await page.goto('/connect')
  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  const connect = navigation.locator('a[href="/connect"]')
  const models = navigation.locator('a[href="/models"]')

  for (const colorScheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme })
    await expect(page.locator('html')).toHaveClass(colorScheme === 'dark' ? /\bdark\b/ : /^(?!.*\bdark\b)/)
    await page.mouse.move(1000, 700)
    await expect(connect).toHaveAttribute('aria-current', 'page')
    await expect(connect).not.toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
    await expect(models).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
  }

  await page.getByRole('button', { name: 'Collapse navigation' }).click()
  await expect(connect).not.toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
  await models.click()
  await page.mouse.move(1000, 700)
  await expect(models).toHaveAttribute('aria-current', 'page')
  await expect(models).not.toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')
  await expect(connect).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)')

  await page.setViewportSize({ width: 500, height: 800 })
  await page.getByRole('button', { name: 'Open navigation' }).click()
  await expect(page.getByRole('dialog').locator('a[href="/models"]')).not.toHaveCSS(
    'background-color',
    'rgba(0, 0, 0, 0)',
  )
})

test('sidebar stays usable in expanded and compact modes', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 })
  await page.goto('/settings')
  await expect(page.getByRole('heading', { name: 'Settings', exact: true })).toBeVisible()
  const firstLevelBreadcrumb = page.getByRole('navigation', { name: 'Breadcrumb' })
  await expect(firstLevelBreadcrumb).toHaveText('Settings')
  await expect(firstLevelBreadcrumb.getByText('Settings')).toHaveAttribute('aria-current', 'page')

  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  const main = page.getByRole('main')
  const expandedContent = await main.boundingBox()
  await page.getByRole('button', { name: 'Collapse navigation' }).click()
  await expect(page.getByRole('button', { name: 'Expand navigation' })).toHaveAttribute('aria-expanded', 'false')
  await expect.poll(async () => (await main.boundingBox())!.width).toBeGreaterThan(expandedContent!.width)
  await expect(navigation.getByRole('link', { name: 'Settings', exact: true })).toHaveAttribute('aria-current', 'page')
  const providerItem = navigation.getByRole('link', { name: 'Providers', exact: true })
  await expect(providerItem).toHaveAttribute('href', '/providers')
  await providerItem.click()
  await expect(page).toHaveURL(/\/providers$/)
  await page.goBack()
  await expect(page).toHaveURL(/\/settings$/)
  await page.getByRole('button', { name: 'Expand navigation' }).click()
  await expect(page.getByRole('button', { name: 'Collapse navigation' })).toHaveAttribute('aria-expanded', 'true')
})

test('sidebar preferences survive reloads and ignore shortcuts while editing', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 })
  await page.goto('/settings')
  await page.getByRole('button', { name: 'Collapse navigation' }).click()
  await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-sidebar-state'))).toBe('collapsed')
  await page.reload()
  await expect(page.getByRole('button', { name: 'Expand navigation' })).toBeVisible()

  const input = page.getByRole('main').locator('input:not([type="hidden"]):enabled').first()
  await input.focus()
  await page.keyboard.press('Control+b')
  await page.keyboard.press('Meta+b')
  await expect(input).toBeFocused()
  await expect(page.getByRole('button', { name: 'Expand navigation' })).toBeVisible()

  await page.getByRole('button', { name: 'Expand navigation' }).focus()
  await page.keyboard.press('Control+b')
  await expect(page.getByRole('button', { name: 'Collapse navigation' })).toBeVisible()
  await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-sidebar-state'))).toBe('expanded')
  await page.reload()
  await expect(page.getByRole('button', { name: 'Collapse navigation' })).toBeVisible()
  await page.getByRole('button', { name: 'Collapse navigation' }).focus()
  await page.keyboard.press('Meta+b')
  await expect(page.getByRole('button', { name: 'Expand navigation' })).toBeVisible()

  const link = page
    .getByRole('navigation', { name: 'Primary navigation' })
    .getByRole('link', { name: 'Models', exact: true })
  await link.focus()
  await page.keyboard.press('Control+b')
  await expect(link).toBeFocused()
  await page.keyboard.press('Meta+b')
  await expect(link).toBeFocused()
})

test('mobile navigation closes on Escape and selection and restores its trigger', async ({ page }) => {
  await page.setViewportSize({ width: 767, height: 800 })
  await page.goto('/settings')
  const trigger = page.getByRole('button', { name: 'Open navigation' })
  await trigger.click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toBeVisible()
  await expect(dialog.getByRole('link', { name: 'Chat', exact: true })).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  await expect(trigger).toBeFocused()
  await trigger.click()
  await dialog.getByRole('link', { name: 'Models', exact: true }).click()
  await expect(page).toHaveURL(/\/models$/)
  await expect(dialog).toHaveCount(0)
  await expect(trigger).toBeFocused()
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  await trigger.click()
  await page.setViewportSize({ width: 768, height: 800 })
  await expect(dialog).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Collapse navigation' })).toBeVisible()
})

test('nested breadcrumbs keep real parent destinations', async ({ page }) => {
  await page.goto('/models/new')
  const breadcrumb = page.getByRole('navigation', { name: 'Breadcrumb' })
  const parent = breadcrumb.getByRole('link', { name: 'Models', exact: true })
  await expect(parent).toHaveAttribute('href', '/models')
  await expect(breadcrumb.getByText('Create', { exact: true })).toHaveAttribute('aria-current', 'page')
  await parent.click()
  await expect(page).toHaveURL(/\/models$/)
  await page.goBack()
  await expect(page).toHaveURL(/\/models\/new$/)
})

test('settings lists sections in the main content instead of nesting another sidebar', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 })
  await page.goto('/settings')

  await expect(page.getByRole('navigation', { name: 'Settings sections' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Select Settings section' })).toHaveCount(0)
  await expect(page.getByRole('region', { name: 'Appearance' })).toHaveCount(1)
  await expect(page.getByRole('region', { name: 'Proxy' })).toHaveCount(1)
  await expect(page.getByRole('region', { name: 'Request history' })).toHaveCount(1)

  await page.setViewportSize({ width: 1500, height: 800 })
  await expect(page.getByRole('region', { name: 'Appearance' })).toBeVisible()
})

test('desktop port controls stay hidden outside the Tauri shell', async ({ page }) => {
  await page.goto('/settings')
  await expect(page.getByRole('heading', { name: 'Desktop', exact: true })).toHaveCount(0)

  await page.goto('/')
  await expect(page.getByRole('heading', { name: 'Fixed desktop port unavailable' })).toHaveCount(0)
})

test('system theme follows browser color-scheme changes', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'light' })
  await page.goto('/settings')
  await expect(page.locator('html')).not.toHaveClass(/\bdark\b/)
  await page.locator('#theme-preference').click()
  await page.getByRole('option', { name: 'System' }).click()

  await page.emulateMedia({ colorScheme: 'dark' })
  await expect(page.locator('html')).toHaveClass(/\bdark\b/)

  await page.locator('#theme-preference').click()
  await page.getByRole('option', { name: 'Light' }).click()
  await expect(page.locator('html')).not.toHaveClass(/\bdark\b/)
  await expect.poll(() => page.evaluate(() => localStorage.getItem('stravia-theme'))).toBe('light')
})

test('settings sections only scroll the main content', async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 800 })
  await page.goto('/settings')

  const trigger = page.getByRole('button', { name: 'Collapse navigation' })
  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  const initialTrigger = await trigger.boundingBox()
  const initialNavigation = await navigation.boundingBox()
  const requestHistory = page.getByRole('region', { name: 'Request history' })
  await requestHistory.scrollIntoViewIfNeeded()
  await expect(requestHistory).toBeVisible()
  await expect.poll(() => page.getByRole('main').evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
  await expect.poll(async () => (await trigger.boundingBox())?.y).toBe(initialTrigger?.y)
  await expect.poll(async () => (await navigation.boundingBox())?.y).toBe(initialNavigation?.y)
  await trigger.click()
  await expect(page.getByRole('button', { name: 'Expand navigation' })).toBeVisible()
})

const standardPages = [
  '/allowances',
  '/api-keys',
  '/connect',
  '/logs',
  '/media-generation',
  '/media-understanding',
  '/models',
  '/providers',
  '/reversible-redaction',
  '/settings',
  '/stats',
  '/vendor-plugins',
  '/web-search',
]

for (const path of standardPages) {
  test(`page header on ${path} is one title row without eyebrow or description`, async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.route('**/api/v1/media-understanding', (route) =>
      route.fulfill({ json: { data: { enabled: false, state: 'disabled', eligible_models: [] } } }),
    )
    await page.route('**/api/v1/reversible-redaction/rules', (route) =>
      route.fulfill({ json: { data: { rules: [], prefilter: '', filter: '' } } }),
    )
    await page.route('**/api/v1/reversible-redaction/discoveries**', (route) =>
      route.fulfill({ json: { data: { items: [], next_cursor: null, observation_gap: false } } }),
    )
    await page.goto(path)
    const title = page.getByRole('heading', { level: 1 })
    await expect(title).toHaveCount(1)
    const header = page.locator('header').filter({ has: title })
    await expect(header.locator('p')).toHaveCount(0)
    const box = (await header.boundingBox())!
    expect(box.height).toBeLessThan(48)
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}

test('All conversations keeps only the one-sentence local history risk under its title', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/conversations')
  const title = page.getByRole('heading', { level: 1, name: 'All conversations' })
  await expect(title).toBeVisible()
  const note = page.locator('header').filter({ has: title }).locator('p')
  await expect(note).toHaveCount(1)
  await expect(note).toHaveText('History stays in this browser; clearing site data deletes it.')
})

test('page header status sits on the title row and stays after the title on narrow screens', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/logs')
  const title = page.getByRole('heading', { level: 1 })
  const header = page.locator('header').filter({ has: title })
  const status = header.getByRole('status')
  await expect(status).toBeVisible()
  const wideTitle = (await title.boundingBox())!
  const wideStatus = (await status.boundingBox())!
  expect(wideStatus.x).toBeGreaterThanOrEqual(wideTitle.x + wideTitle.width)
  expect(Math.abs(wideStatus.y + wideStatus.height / 2 - (wideTitle.y + wideTitle.height / 2))).toBeLessThan(
    wideTitle.height / 2,
  )

  await page.setViewportSize({ width: 320, height: 800 })
  const narrowTitle = (await title.boundingBox())!
  const narrowStatus = (await status.boundingBox())!
  expect(
    narrowStatus.x >= narrowTitle.x + narrowTitle.width || narrowStatus.y >= narrowTitle.y + narrowTitle.height - 1,
  ).toBe(true)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})

test('content container caps at 120rem, centers, and keeps one left edge across list and detail pages', async ({
  page,
}) => {
  const container = page.getByRole('main').locator('> div')
  const containerBox = async () => {
    const box = await container.boundingBox()
    return box && { x: Math.round(box.x), width: Math.round(box.width) }
  }

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/providers')
  await expect(page.getByRole('heading', { level: 1 })).toBeVisible()
  await expect.poll(containerBox).toEqual({ x: 282, width: 1124 })

  await page.setViewportSize({ width: 2560, height: 1440 })
  // 展开侧栏时主区 2296px，内容达到 120rem 上限，两侧各留 188px。
  await expect.poll(containerBox).toEqual({ x: 256 + 188, width: 1920 })
  await page.getByRole('button', { name: 'Collapse navigation' }).click()
  // 折叠侧栏后主区 2504px，容器仍是 1920px，仅向左移动 104px。
  await expect.poll(containerBox).toEqual({ x: 48 + 292, width: 1920 })

  await page.getByRole('button', { name: 'Expand navigation' }).click()
  const titleLeft: Record<string, number> = {}
  for (const path of ['/providers', '/providers/missing-provider', '/models', '/models/new']) {
    await page.goto(path)
    const title = page.getByRole('main').getByRole('heading', { level: 1 })
    await expect(title).toBeVisible()
    titleLeft[path] = Math.round((await title.boundingBox())!.x)
  }
  expect(new Set(Object.values(titleLeft)).size, JSON.stringify(titleLeft)).toBe(1)
  await expect.poll(containerBox).toMatchObject({ x: titleLeft['/providers'], width: 1920 })
})
