import { expect, test, type Page, type Request } from '@playwright/test'

import { communityCapabilities, prepareApp } from './prepare-app'

const builtInNavigation = [
  '/',
  '/conversations',
  '/providers',
  '/models',
  '/api-keys',
  '/connect',
  '/media-understanding',
  '/media-generation',
  '/web-search',
  '/reversible-redaction',
  '/logs',
  '/stats',
  '/allowances',
  '/vendor-plugins',
  '/settings',
]

const copy = {
  'en-US': {
    settings: 'Settings',
    primaryNavigation: 'Primary navigation',
    openNavigation: 'Open navigation',
    providers: 'Providers',
    settingsSections: 'Settings sections',
    settingsGroups: ['General', 'Network', 'Data', 'Account', 'Advanced'],
    loadFailedTitle: "Couldn't load edition features",
    loadFailedDescription: 'Navigation may be missing features of your edition until they load.',
    retry: 'Retry',
  },
  'zh-CN': {
    settings: '设置',
    primaryNavigation: '主导航',
    openNavigation: '打开导航',
    providers: '供应商',
    settingsSections: '设置分组',
    settingsGroups: ['通用', '网络', '数据', '账号', '高级'],
    loadFailedTitle: '无法加载版本功能',
    loadFailedDescription: '加载成功前，导航中可能缺少当前版本的功能。',
    retry: '重试',
  },
} as const

function isCommercialRequest(request: Request): boolean {
  const path = new URL(request.url()).pathname
  return path.startsWith('/api/v1/ee/') || /\/api\/v1\/(license|commercial)\b/.test(path)
}

async function expectNoHorizontalOverflow(page: Page): Promise<void> {
  await expect
    .poll(() =>
      page.evaluate(() => {
        const main = document.querySelector('main')
        return document.documentElement.scrollWidth <= innerWidth && (!main || main.scrollWidth <= main.clientWidth)
      }),
    )
    .toBe(true)
}

async function useLocale(page: Page, locale: keyof typeof copy): Promise<void> {
  await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

for (const locale of ['en-US', 'zh-CN'] as const) {
  test(`Community Edition renders clean navigation and settings without commercial UI in ${locale}`, async ({
    page,
  }) => {
    const text = copy[locale]
    const commercialRequests: string[] = []
    page.on('request', (request) => {
      if (isCommercialRequest(request)) commercialRequests.push(request.url())
    })
    await useLocale(page, locale)
    await page.setViewportSize({ width: 1280, height: 800 })

    const capabilitiesResponse = page.waitForResponse((response) =>
      new URL(response.url()).pathname.endsWith('/api/v1/capabilities'),
    )
    await page.goto('/settings')
    const response = await capabilitiesResponse
    expect(response.ok()).toBe(true)
    expect(await response.json()).toEqual(communityCapabilities)
    await expect(page.getByRole('heading', { name: text.settings, exact: true })).toBeVisible()

    const navigation = page.getByRole('navigation', { name: text.primaryNavigation })
    await expect(navigation.getByRole('link', { name: text.settings, exact: true })).toBeVisible()
    const hrefs = await navigation
      .locator('a[href]')
      .evaluateAll((links) => links.map((link) => new URL((link as HTMLAnchorElement).href).pathname))
    expect(hrefs.filter((href) => !builtInNavigation.includes(href))).toEqual([])

    const settingsNav = page.getByRole('navigation', { name: text.settingsSections })
    await expect(settingsNav.getByRole('link')).toHaveText([...text.settingsGroups])

    await expect(page.locator('[data-slot="shell-notices"]')).toHaveCount(0)
    await expect(page.locator('[data-extension-slot]')).toHaveCount(0)
    await expect(page.locator('[data-extension-settings-group]')).toHaveCount(0)
    await expect(page.locator('#edition')).toHaveCount(0)
    await expectNoHorizontalOverflow(page)

    await navigation.getByRole('link', { name: text.providers, exact: true }).click()
    await expect(page).toHaveURL(/\/providers$/)
    await expect(page.locator('[data-slot="shell-notices"]')).toHaveCount(0)

    await page.setViewportSize({ width: 390, height: 800 })
    await page.goto('/settings')
    await expect(page.getByRole('heading', { name: text.settings, exact: true })).toBeVisible()
    await expect(page.locator('[data-slot="shell-notices"]')).toHaveCount(0)
    await expect(page.locator('[data-extension-slot]')).toHaveCount(0)
    await expectNoHorizontalOverflow(page)

    await page.getByRole('button', { name: text.openNavigation }).click()
    await page.getByRole('dialog').getByRole('link', { name: text.providers, exact: true }).click()
    await expect(page).toHaveURL(/\/providers$/)
    await expectNoHorizontalOverflow(page)
    expect(commercialRequests).toEqual([])
  })

  test(`capabilities failure shows a retryable error alert and recovers cleanly in ${locale}`, async ({ page }) => {
    const text = copy[locale]
    let failing = true
    let attempts = 0
    await useLocale(page, locale)
    await page.route('**/api/v1/capabilities', async (route) => {
      attempts += 1
      if (failing) {
        await route.fulfill({
          status: 500,
          json: { error: { code: 'INTERNAL_ERROR', message: 'capability provider unavailable' } },
        })
        return
      }
      await route.fulfill({ json: communityCapabilities })
    })

    await page.setViewportSize({ width: 390, height: 800 })
    await page.goto('/settings')
    const failure = page.getByRole('alert').filter({ hasText: text.loadFailedTitle })
    await expect(failure).toBeVisible()
    await expect(failure).toContainText(text.loadFailedDescription)
    await expect(failure).toContainText('capability provider unavailable')
    await expectNoHorizontalOverflow(page)

    const failureBox = await failure.boundingBox()
    expect(failureBox!.x).toBeGreaterThanOrEqual(0)
    expect(failureBox!.x + failureBox!.width).toBeLessThanOrEqual(390)

    failing = false
    await failure.getByRole('button', { name: text.retry, exact: true }).click()
    await expect(failure).toHaveCount(0)
    await expect(page.locator('[data-slot="shell-notices"]')).toHaveCount(0)
    await expect(page.locator('[data-extension-slot]')).toHaveCount(0)
    expect(attempts).toBeGreaterThanOrEqual(2)
    await expect(page.getByRole('heading', { name: text.settings, exact: true })).toBeVisible()
    await expectNoHorizontalOverflow(page)
  })
}
