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

  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  const main = page.getByRole('main')
  const expandedContent = await main.boundingBox()
  await page.getByRole('button', { name: 'Collapse navigation' }).click()
  await expect(page.getByRole('button', { name: 'Expand navigation' })).toHaveAttribute('aria-expanded', 'false')
  await expect.poll(async () => (await main.boundingBox())!.width).toBeGreaterThan(expandedContent!.width)
  await expect(navigation.getByRole('link', { name: 'Settings', exact: true })).toHaveAttribute('aria-current', 'page')
  const providerItem = navigation.getByRole('link', { name: 'Model services', exact: true })
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
  await expect(dialog.getByRole('link', { name: 'Overview', exact: true })).toBeFocused()
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
