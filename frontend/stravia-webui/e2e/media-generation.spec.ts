import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('media generation saves its gate independently from a route draft and recovers from a rejected route', async ({
  page,
}) => {
  let config = { enabled: false, image: { route_id: 'route-a' as string | null } }
  let validation = { valid: true, code: null as string | null, message: null as string | null }
  const writes: (typeof config)[] = []
  let rejectRouteBOnce = true

  await page.route('**/api/v1/media-generation/config', async (route) => {
    if (route.request().method() === 'GET') {
      await route.fulfill({ json: { data: { config, validation } } })
      return
    }

    const input = route.request().postDataJSON() as typeof config
    writes.push(structuredClone(input))
    if (input.image.route_id === 'route-b' && rejectRouteBOnce) {
      rejectRouteBOnce = false
      await route.fulfill({
        status: 400,
        json: {
          error: 'The selected route contains an incompatible destination.',
          code: 'media_generation_target_incompatible',
        },
      })
      return
    }

    config = input
    validation = { valid: true, code: null, message: null }
    await route.fulfill({ json: { data: { config, validation } } })
  })
  await page.route('**/api/v1/media-generation/eligible-routes', async (route) => {
    await route.fulfill({
      json: {
        data: [
          { id: 'route-a', name: 'Image Route A' },
          { id: 'route-b', name: 'Image Route B' },
        ],
      },
    })
  })

  await page.goto('/media-generation')
  await expect(page.getByRole('heading', { name: 'Media generation', exact: true })).toBeVisible()
  await page
    .getByRole('navigation', { name: 'Primary navigation' })
    .getByRole('link', { name: /Media generation/ })
    .click()
  await expect(page).toHaveURL(/\/media-generation$/)

  const routeSelect = page.getByRole('button', { name: 'Model Route', exact: true })
  await routeSelect.click()
  await page.getByRole('option', { name: 'Image Route B' }).click()
  await expect(page.getByText('The route selection has unsaved changes.')).toBeVisible()

  await page.getByRole('switch', { name: 'Enable media generation' }).click()
  await expect.poll(() => writes.length).toBe(1)
  expect(writes[0]).toEqual({ enabled: true, image: { route_id: 'route-a' } })
  await expect(page.getByRole('switch', { name: 'Enable media generation' })).toBeChecked()
  await expect(routeSelect).toContainText('Image Route B')

  await page.getByRole('button', { name: 'Save settings' }).click()
  await expect(page.getByText(/not compatible with image generation/i)).toBeVisible()
  await expect(routeSelect).toContainText('Image Route B')
  expect(config).toEqual({ enabled: true, image: { route_id: 'route-a' } })

  await page.getByRole('button', { name: 'Save settings' }).click()
  await expect(page.getByText('Media generation settings saved.')).toBeVisible()
  await expect.poll(() => config).toEqual({ enabled: true, image: { route_id: 'route-b' } })
})

test('empty media generation configuration stays usable in Chinese dark mode at 320px', async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem('stravia-locale', 'zh-CN')
    localStorage.setItem('stravia-theme', 'system')
  })
  await page.emulateMedia({ colorScheme: 'dark' })
  await page.setViewportSize({ width: 320, height: 760 })
  await page.route('**/api/v1/media-generation/config', async (route) => {
    await route.fulfill({
      json: {
        data: {
          config: { enabled: false, image: { route_id: null } },
          validation: { valid: false, code: 'media_generation_route_missing', message: null },
        },
      },
    })
  })
  await page.route('**/api/v1/media-generation/eligible-routes', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })

  await page.goto('/media-generation')

  await expect(page.locator('html')).toHaveClass(/\bdark\b/)
  await expect(page.getByRole('heading', { name: '媒体生成', exact: true })).toBeVisible()
  await expect(page.getByRole('switch', { name: '启用媒体生成' })).toBeDisabled()
  await expect(page.getByRole('link', { name: '管理模型', exact: true }).first()).toHaveAttribute('href', '/models')
  await expect(page.getByRole('link', { name: '管理模型服务', exact: true })).toHaveAttribute('href', '/providers')
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})
