import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('sidebar stays usable in expanded and compact modes', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 })
  await page.goto('/settings')
  await expect(page.getByRole('heading', { name: 'Settings', exact: true })).toBeVisible()
  const firstLevelBreadcrumb = page.getByRole('navigation', { name: 'Breadcrumb' })
  await expect(firstLevelBreadcrumb).toHaveText('Settings')
  await expect(firstLevelBreadcrumb.getByText('Settings')).toHaveAttribute('aria-current', 'page')

  const sidebar = page.locator('aside').first()
  const trigger = page.getByRole('button', { name: 'Collapse navigation' })
  await expect(sidebar).toHaveCSS('width', '256px')
  await trigger.click()
  await expect(sidebar).toHaveCSS('width', '48px')

  const activeItem = sidebar.getByRole('link', { name: 'Settings' })
  await expect(activeItem).toHaveAttribute('aria-current', 'page')
  await expect
    .poll(async () => {
      const box = await activeItem.boundingBox()
      return box && { width: box.width, height: box.height }
    })
    .toEqual({ width: 40, height: 40 })

  const providerItem = sidebar.getByRole('link', { name: 'Model services' })
  const idleBackground = await providerItem.evaluate((element) => getComputedStyle(element).backgroundColor)
  await providerItem.hover()
  await expect
    .poll(() => providerItem.evaluate((element) => getComputedStyle(element).backgroundColor))
    .not.toBe(idleBackground)
})

test('settings uses one compact section control instead of nesting another sidebar', async ({ page }) => {
  await page.setViewportSize({ width: 1200, height: 800 })
  await page.goto('/settings')

  const sectionSelect = page.getByRole('button', { name: 'Select Settings section' })
  await expect(sectionSelect).toBeVisible()
  await expect(page.getByRole('navigation', { name: 'Settings sections' })).toHaveCount(0)

  await page.setViewportSize({ width: 1500, height: 800 })
  await expect(sectionSelect).toBeVisible()
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

test('settings section selection only scrolls the main content', async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 800 })
  await page.goto('/settings')

  const sectionSelect = page.getByRole('button', { name: 'Select Settings section' })
  await sectionSelect.click()
  await page.getByRole('option', { name: 'Request history' }).click()
  await expect(sectionSelect).toHaveText('Request history')

  await expect
    .poll(() =>
      page.evaluate(() => {
        const shell = document.querySelector<HTMLElement>('.shell-root')
        const header = document.querySelector<HTMLElement>('header')
        const sidebar = document.querySelector<HTMLElement>('aside')
        const main = document.querySelector<HTMLElement>('main')
        const logs = document.getElementById('logs')
        const mainRect = main?.getBoundingClientRect()
        const logsRect = logs?.getBoundingClientRect()
        return {
          shellScrollTop: shell?.scrollTop,
          headerTop: header?.getBoundingClientRect().top,
          sidebarTop: sidebar?.getBoundingClientRect().top,
          mainScrolled: (main?.scrollTop ?? 0) > 0,
          logsVisible: Boolean(mainRect && logsRect && logsRect.top >= mainRect.top && logsRect.top < mainRect.bottom),
        }
      }),
    )
    .toEqual({ shellScrollTop: 0, headerTop: 0, sidebarTop: 40, mainScrolled: true, logsVisible: true })
})
