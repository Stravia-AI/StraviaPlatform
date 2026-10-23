import { expect, test } from '@playwright/test'
import type { Page, Route } from '@playwright/test'

import { prepareApp } from './prepare-app'

const apiKey = {
  id: 'key-draft',
  key: 'sk-draft-secret',
  name: 'Draft client',
  rpm_limit: null,
  is_enabled: true,
  mcp_access_enabled: false,
  transparent_injection_enabled: false,
  inject_web_search: false,
  inject_media_understanding: false,
  inject_media_generation: false,
  expires_at: null,
  created_at: '2026-08-17T00:00:00Z',
  updated_at: '2026-08-17T00:00:00Z',
  model_ids: [],
}

async function mockApiKeys(page: Page, options: { update?: (route: Route) => Promise<void> }) {
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [] } }))
  await page.route('**/api/v1/api-keys', (route) => route.fulfill({ json: { data: [apiKey] } }))
  await page.route('**/api/v1/api-keys/key-draft', async (route) => {
    if (options.update) await options.update(route)
    else await route.fulfill({ json: { data: apiKey } })
  })
  await page.route('**/api/v1/web-search/config', (route) => route.fulfill({ json: { data: { enabled: false } } }))
  await page.route('**/api/v1/media-understanding', (route) =>
    route.fulfill({ json: { data: { image: { enabled: false }, audio: { enabled: false } } } }),
  )
  await page.route('**/api/v1/media-generation/config', (route) =>
    route.fulfill({
      json: {
        data: {
          config: { enabled: false, image: { route_id: null } },
          validation: { valid: false, code: 'media_generation_route_missing', message: null },
        },
      },
    }),
  )
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
  await page.setViewportSize({ width: 1280, height: 800 })
})

async function openEditor(page: Page) {
  await page.goto('/api-keys')
  await page.getByRole('row').filter({ hasText: 'Draft client' }).getByRole('cell').nth(1).click()
  const editor = page.locator('[data-slot="sheet-content"]')
  await expect(editor.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()
  return editor
}

const discardDialog = (page: Page) => page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })

test('without a draft every close path closes the Sheet directly', async ({ page }) => {
  await mockApiKeys(page, {})
  const editor = await openEditor(page)

  await page.keyboard.press('Escape')
  await expect(editor).toBeHidden()
  await expect(discardDialog(page)).toHaveCount(0)

  await openEditor(page)
  await page.mouse.click(8, 400)
  await expect(editor).toBeHidden()

  await openEditor(page)
  await editor.getByRole('button', { name: 'Close API Key editor' }).click()
  await expect(editor).toBeHidden()

  await openEditor(page)
  await editor.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(editor).toBeHidden()
  await expect(discardDialog(page)).toHaveCount(0)
})

test('with a draft Escape, overlay, close button and Cancel confirm before discarding', async ({ page }) => {
  await mockApiKeys(page, {})
  const editor = await openEditor(page)
  const name = editor.getByLabel('Name', { exact: true })
  await name.fill('Renamed client')

  const closers: Array<[string, () => Promise<void>]> = [
    ['Escape', () => page.keyboard.press('Escape')],
    ['overlay', () => page.mouse.click(8, 400)],
    ['close button', () => editor.getByRole('button', { name: 'Close API Key editor' }).click()],
    ['Cancel', () => editor.getByRole('button', { name: 'Cancel', exact: true }).click()],
  ]
  for (const [, close] of closers) {
    await close()
    await expect(discardDialog(page)).toBeVisible()
    await expect(editor).toBeVisible()
    await discardDialog(page).getByRole('button', { name: 'Keep editing' }).click()
    await expect(discardDialog(page)).toBeHidden()
    await expect(editor).toBeVisible()
    await expect(name).toHaveValue('Renamed client')
  }

  await page.keyboard.press('Escape')
  await discardDialog(page).getByRole('button', { name: 'Discard changes' }).click()
  await expect(editor).toBeHidden()

  // 放弃后重新打开是已保存的值。
  await page.getByRole('row').filter({ hasText: 'Draft client' }).getByRole('cell').nth(1).click()
  await expect(editor.getByLabel('Name', { exact: true })).toHaveValue('Draft client')
})

test('saving locks the Sheet and a failed save keeps the draft', async ({ page }) => {
  const { promise: gate, resolve: release } = Promise.withResolvers<void>()
  await mockApiKeys(page, {
    update: async (route) => {
      await gate
      await route.fulfill({ status: 500, json: { error: { message: 'Save rejected', code: 'internal_error' } } })
    },
  })
  const editor = await openEditor(page)
  const name = editor.getByLabel('Name', { exact: true })
  await name.fill('Unsaved rename')
  await editor.getByRole('button', { name: 'Save API Key' }).click()

  await expect(editor.getByRole('button', { name: 'Close API Key editor' })).toBeDisabled()
  await expect(editor.getByRole('button', { name: 'Cancel', exact: true })).toBeDisabled()
  await page.keyboard.press('Escape')
  await page.mouse.click(8, 400)
  await expect(editor).toBeVisible()
  await expect(discardDialog(page)).toHaveCount(0)

  release()
  await expect(editor.getByRole('button', { name: 'Close API Key editor' })).toBeEnabled()
  await expect(name).toHaveValue('Unsaved rename')
  await page.keyboard.press('Escape')
  await expect(discardDialog(page)).toBeVisible()
})

test('leaving the route with a draft asks first, for links and browser history', async ({ page }) => {
  await mockApiKeys(page, {})
  await page.goto('/api-keys/key-draft')
  const name = page.getByLabel('Name', { exact: true })
  await expect(name).toHaveValue('Draft client')

  // 无草稿：直接离开。
  await page.locator('a[href="/providers"]').first().click()
  await expect(page).toHaveURL(/\/providers$/)
  await page.goBack()
  await expect(page).toHaveURL(/\/api-keys\/key-draft$/)

  await name.fill('Renamed client')
  await page.locator('a[href="/providers"]').first().click()
  await expect(discardDialog(page)).toBeVisible()
  await expect(page).toHaveURL(/\/api-keys\/key-draft$/)
  await discardDialog(page).getByRole('button', { name: 'Keep editing' }).click()
  await expect(page).toHaveURL(/\/api-keys\/key-draft$/)
  await expect(name).toHaveValue('Renamed client')

  // 浏览器前进/后退同样先确认，确认后再前往目的地。
  await page.goForward()
  await expect(discardDialog(page)).toBeVisible()
  await expect(page).toHaveURL(/\/api-keys\/key-draft$/)
  await discardDialog(page).getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/\/providers$/)
})
