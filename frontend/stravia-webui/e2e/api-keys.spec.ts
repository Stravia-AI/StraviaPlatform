import { expect, test } from '@playwright/test'
import type { Locator, Page, Route } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('API Key editor preserves disabled transparent injection selections', async ({ page }) => {
  let writes = 0
  let persistedKey = {
    id: 'key-advanced',
    key: 'sk-advanced',
    name: 'Advanced client',
    rpm_limit: null,
    is_enabled: true,
    mcp_access_enabled: true,
    transparent_injection_enabled: true,
    inject_media_understanding: true,
    inject_media_generation: true,
    inject_web_search: true,
    expires_at: null,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
    model_ids: [],
  }

  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })
  await page.route('**/api/v1/api-keys/key-advanced', async (route) => {
    writes += 1
    const input = route.request().postDataJSON()
    persistedKey = { ...persistedKey, ...input }
    await route.fulfill({ json: { data: persistedKey } })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({ json: { data: [persistedKey] } })
  })
  await page.route('**/api/v1/web-search/config', async (route) => {
    await route.fulfill({ json: { data: { enabled: false } } })
  })
  await page.route('**/api/v1/media-understanding', async (route) => {
    await route.fulfill({ json: { data: { image: { enabled: false }, audio: { enabled: false } } } })
  })
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

  await page.setViewportSize({ width: 1279, height: 800 })
  await page.goto('/api-keys')
  const tableContainer = page.locator('[data-slot="table-container"]')
  await expect(tableContainer).toBeVisible()
  expect(await tableContainer.evaluate((element) => element.scrollWidth)).toBeLessThanOrEqual(
    await tableContainer.evaluate((element) => element.clientWidth),
  )
  await page.getByRole('row').filter({ hasText: 'Advanced client' }).getByRole('cell').nth(1).click()
  const editor = page.locator('[data-slot="sheet-content"]')
  await expect(editor.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()
  await expect(page).toHaveURL(/\/api-keys$/)
  const secret = editor.getByLabel('API key', { exact: true })
  await expect(secret).toHaveAttribute('type', 'password')
  await secret.fill('sk-edited-secret')
  await editor.getByRole('button', { name: 'Show API key', exact: true }).click()
  await expect(secret).toHaveAttribute('type', 'text')
  await expect(secret).toHaveValue('sk-edited-secret')
  await editor.getByRole('button', { name: 'Hide API key', exact: true }).click()
  await expect(secret).toHaveAttribute('type', 'password')
  await expect(secret).toHaveValue('sk-edited-secret')
  await editor.getByRole('button', { name: 'Show API key', exact: true }).click()
  await editor.getByRole('button', { name: 'Cancel', exact: true }).click()
  // 编辑过密钥的草稿：取消先确认，放弃后才关闭。
  await page
    .getByRole('alertdialog', { name: 'Discard unsaved changes?' })
    .getByRole('button', { name: 'Discard changes' })
    .click()
  await expect(editor).toBeHidden()
  expect(writes).toBe(0)
  await page.getByRole('row').filter({ hasText: 'Advanced client' }).getByRole('cell').nth(1).click()
  await expect(secret).toHaveAttribute('type', 'password')
  await expect(secret).toHaveValue('sk-advanced')
  await expect(page.locator('#api-key-mcp-access')).toHaveAttribute('aria-checked', 'true')
  await expect(page.locator('#api-key-transparent-injection')).toHaveAttribute('aria-checked', 'true')
  const mediaSelection = page.locator('#api-key-inject-media-understanding')
  const generationSelection = page.locator('#api-key-inject-media-generation')
  const searchSelection = page.locator('#api-key-inject-web-search')
  await expect(mediaSelection).toHaveAttribute('aria-checked', 'true')
  await expect(generationSelection).toHaveAttribute('aria-checked', 'true')
  await expect(searchSelection).toHaveAttribute('aria-checked', 'true')
  await expect(mediaSelection).toBeDisabled()
  await expect(generationSelection).toBeDisabled()
  await expect(searchSelection).toBeDisabled()
  await expect(
    page.getByRole('link', { name: 'Turn on this feature in Advanced Features before Stravia can expose it.' }),
  ).toHaveCount(3)
  await page.getByRole('button', { name: 'Save API Key' }).click()

  expect(persistedKey.inject_media_understanding).toBe(true)
  expect(persistedKey.inject_media_generation).toBe(true)
  expect(persistedKey.inject_web_search).toBe(true)
  await expect(editor).toBeHidden()
  await expect(page).toHaveURL(/\/api-keys$/)
  await page.reload()
  await page.getByRole('row').filter({ hasText: 'Advanced client' }).getByRole('cell').nth(2).click()
  await expect(page.locator('#api-key-inject-media-understanding')).toHaveAttribute('aria-checked', 'true')
  await expect(page.locator('#api-key-inject-media-generation')).toHaveAttribute('aria-checked', 'true')
  await expect(page.locator('#api-key-inject-web-search')).toHaveAttribute('aria-checked', 'true')
})

test('API Key editor keeps optional help in tooltips and required scope visible at every width', async ({ page }) => {
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })

  for (const width of [1920, 390]) {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/api-keys')
    await page.getByRole('button', { name: 'Create first API Key' }).click()
    const overlay = page.locator('[data-slot="sheet-content"]')
    await expect(overlay.getByRole('heading', { name: 'Create API Key' })).toBeVisible()

    // Optional explanations live only in the question-mark tooltip and never add a visible line under the label.
    await expect(overlay.getByText('Leave empty for unlimited', { exact: false })).toHaveCount(0)
    const rpm = overlay.locator('[data-slot="field"]').filter({ has: page.locator('#api-key-rpm-limit') })
    const help = rpm.getByRole('button', { name: 'More about this field' })
    await help.focus()
    await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Leave empty for unlimited')
    await expect(page.locator('[data-slot="tooltip-content"]')).not.toContainText('internal')
    await page.keyboard.press('Escape')
    await expect(page.locator('[data-slot="tooltip-content"]')).toHaveCount(0)
    // 点击同样可以打开问号提示。
    await help.click()
    await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Leave empty for unlimited')
    await page.keyboard.press('Escape')
    await expect(page.locator('[data-slot="tooltip-content"]')).toHaveCount(0)

    // Access scope is required information and stays visible without hover.
    await expect(overlay.getByText('Lets this key call Stravia tools through MCP.')).toBeVisible()
    await expect(overlay.getByText(/every current and future model/)).toBeVisible()
    await expect(
      overlay.getByText('Adds the selected features to compatible model requests without client changes', {
        exact: false,
      }),
    ).toBeVisible()
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  }
})

test.describe('touch help', () => {
  test.use({ hasTouch: true })

  test('question-mark help opens on tap and closes with Escape', async ({ page }) => {
    await page.route('**/api/v1/models', async (route) => {
      await route.fulfill({ json: { data: [] } })
    })
    await page.route('**/api/v1/api-keys', async (route) => {
      await route.fulfill({ json: { data: [] } })
    })
    await page.setViewportSize({ width: 390, height: 844 })
    await page.goto('/api-keys')
    await page.getByRole('button', { name: 'Create first API Key' }).click()
    const overlay = page.locator('[data-slot="sheet-content"]')
    const rpm = overlay.locator('[data-slot="field"]').filter({ has: page.locator('#api-key-rpm-limit') })
    const help = rpm.getByRole('button', { name: 'More about this field' })
    await help.tap()
    await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Leave empty for unlimited')
    // 轻触触发器不会立刻关闭提示；Escape 关闭。
    await help.tap()
    await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Leave empty for unlimited')
    await page.keyboard.press('Escape')
    await expect(page.locator('[data-slot="tooltip-content"]')).toHaveCount(0)
  })
})

test('API Key editor groups and filters allowed model selections', async ({ page }) => {
  const modelId = '4d93a8ac-0d1f-4891-8780-55c2f566c084'
  const secondModelId = 'd414f418-547e-4269-b917-fec9a10c39bb'
  const modelName = 'gpt-5.6-sol'
  const secondModelName = 'gpt-5.6-luna'
  let modelIds: string[] = []
  let rpmLimit: number | null = null
  const apiKey = {
    id: 'key-test',
    key: 'sk-test-secret',
    name: 'test',
    rpm_limit: null,
    is_enabled: true,
    mcp_access_enabled: true,
    transparent_injection_enabled: true,
    inject_media_understanding: true,
    inject_media_generation: true,
    inject_web_search: true,
    expires_at: null,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }

  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: modelId,
            model_id: modelName,
            display_name: 'GPT 5.6 Sol',
            balance: 'weighted',
            is_enabled: true,
            created_at: '2026-08-17T00:00:00Z',
            targets: [],
          },
          {
            id: secondModelId,
            model_id: secondModelName,
            display_name: null,
            balance: 'priority',
            is_enabled: true,
            created_at: '2026-08-17T00:00:00Z',
            targets: [],
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({ json: { data: [{ ...apiKey, rpm_limit: rpmLimit, model_ids: modelIds }] } })
  })
  await page.route('**/api/v1/api-keys/key-test', async (route) => {
    const input = route.request().postDataJSON()
    modelIds = input.model_ids
    rpmLimit = input.rpm_limit
    await route.fulfill({ json: { data: { ...apiKey, rpm_limit: rpmLimit, model_ids: modelIds } } })
  })

  await page.goto('/api-keys')
  await page.getByRole('row').filter({ hasText: 'test' }).getByRole('cell').nth(2).click()
  const editor = page.locator('[data-slot="sheet-content"]')
  await expect(editor.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()
  await expect(page).toHaveURL(/\/api-keys$/)
  await page.setViewportSize({ width: 493, height: 832 })
  await page.setViewportSize({ width: 544, height: 832 })
  const allowAllModels = page.getByRole('switch', { name: 'Allow all models' })
  await expect(allowAllModels).toBeChecked()
  await expect(page.getByRole('combobox', { name: 'Select allowed models' })).toHaveCount(0)
  await allowAllModels.click()
  await expect(allowAllModels).not.toBeChecked()
  const modelPicker = page.locator('#api-key-model-picker')
  await expect(modelPicker).toContainText('Allowed: 2')
  await modelPicker.click()
  const modelMenu = page.locator('[data-slot="popover-content"]')
  const modelSearch = modelMenu.getByPlaceholder('Search models…')
  await expect(modelSearch).toBeVisible()
  await expect(modelMenu.getByText('GPT 5.6 Sol', { exact: true })).toBeVisible()
  await expect(modelMenu.getByText(modelName, { exact: true })).toBeVisible()
  await modelSearch.fill('GPT 5.6 Sol')
  await expect(modelMenu.getByText(modelName, { exact: true })).toBeVisible()
  await modelSearch.fill('luna')
  await expect(modelMenu.getByText(secondModelName, { exact: true })).toBeVisible()
  await expect(modelMenu.getByText(modelName, { exact: true })).toBeHidden()
  await modelSearch.fill('')
  await modelMenu.getByText(secondModelName, { exact: true }).click()
  await expect(modelPicker).toContainText('Allowed: 1')
  const allowedGroup = modelMenu.getByRole('group', { name: 'Allowed models' })
  const unallowedGroup = modelMenu.getByRole('group', { name: 'Not allowed' })
  await expect(allowedGroup).toBeVisible()
  await expect(unallowedGroup).toBeVisible()
  const [allowedGroupBox, unallowedGroupBox] = await Promise.all([
    allowedGroup.boundingBox(),
    unallowedGroup.boundingBox(),
  ])
  if (!allowedGroupBox || !unallowedGroupBox) throw new Error('Model permission groups are not visible')
  expect(allowedGroupBox.y).toBeLessThan(unallowedGroupBox.y)
  await page.keyboard.press('Escape')
  await page.setViewportSize({ width: 493, height: 832 })

  await expect(page.getByLabel('Requests per minute (RPM)')).toHaveValue('')
  await page.getByLabel('Requests per minute (RPM)').fill('2')
  await page.getByRole('button', { name: 'Save API Key' }).click()
  await expect(page.getByText('API key saved.')).toBeVisible()
  await expect.poll(() => modelIds).toEqual([modelId])
  await expect(editor).toBeHidden()
  await page.setViewportSize({ width: 1280, height: 720 })

  await page.getByRole('row').filter({ hasText: 'test' }).getByRole('cell').nth(3).click()
  const reopenedAllowAllModels = page.getByRole('switch', { name: 'Allow all models' })
  await expect(reopenedAllowAllModels).not.toBeChecked()
  await expect(page.getByRole('combobox', { name: 'Select allowed models' })).toContainText('Allowed: 1')
  await reopenedAllowAllModels.click()
  await expect(reopenedAllowAllModels).toBeChecked()
  await expect(page.locator('#api-key-model-picker')).toHaveCount(0)
  await page.getByRole('button', { name: 'Save API Key' }).click()
  await expect.poll(() => modelIds).toEqual([])
})

test('API Key direct page titles itself with the key name, falls back to its ID, and keeps recovery when missing', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  const apiKey = {
    id: 'key-direct',
    key: 'sk-direct',
    name: 'Direct client',
    rpm_limit: null,
    is_enabled: true,
    mcp_access_enabled: false,
    transparent_injection_enabled: false,
    inject_media_understanding: false,
    inject_media_generation: false,
    inject_web_search: false,
    expires_at: null,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
    model_ids: [],
  }
  const unnamedKey = { ...apiKey, id: 'key-unnamed', key: 'sk-unnamed', name: '' }
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [] } }))
  await page.route('**/api/v1/api-keys', (route) => route.fulfill({ json: { data: [apiKey, unnamedKey] } }))

  await page.goto('/api-keys/key-direct')
  const title = page.getByRole('heading', { level: 1 })
  await expect(title).toHaveText('Direct client')
  await expect(page.getByRole('heading', { name: 'Edit API Key' })).toHaveCount(0)
  await expect(page.locator('header').filter({ has: title }).locator('p')).toHaveCount(0)

  await page.goto('/api-keys/key-unnamed')
  await expect(title).toHaveText('key-unnamed')

  await page.goto('/api-keys/key-missing')
  await expect(title).toHaveText('API Key not found')
  await expect(page.getByRole('link', { name: 'Back to API Keys' })).toHaveAttribute('href', '/api-keys')
})

const advancedKey = {
  id: 'key-layout',
  key: 'sk-layout-secret',
  name: 'Layout client',
  rpm_limit: 60,
  is_enabled: true,
  mcp_access_enabled: true,
  transparent_injection_enabled: true,
  inject_media_understanding: false,
  inject_media_generation: false,
  inject_web_search: true,
  expires_at: null,
  created_at: '2026-08-17T00:00:00Z',
  updated_at: '2026-08-17T00:00:00Z',
  model_ids: [],
}

async function mockLayoutKey(page: Page, update?: (route: Route) => Promise<void>) {
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [] } }))
  await page.route('**/api/v1/api-keys', (route) => route.fulfill({ json: { data: [advancedKey] } }))
  await page.route('**/api/v1/api-keys/key-layout', async (route) => {
    if (update) await update(route)
    else await route.fulfill({ json: { data: advancedKey } })
  })
  await page.route('**/api/v1/web-search/config', (route) => route.fulfill({ json: { data: { enabled: true } } }))
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

const editorControlIds = (scope: Locator) =>
  scope.evaluate((root) =>
    [...root.querySelectorAll('input[id], button[role="combobox"][id], button[role="switch"][id]')]
      .map((element) => element.id)
      .filter((id) => id.startsWith('api-key-')),
  )

test('API Key direct page and Sheet share field order and semantic widths', async ({ page }) => {
  await mockLayoutKey(page)

  await page.setViewportSize({ width: 2560, height: 1440 })
  await page.goto('/api-keys/key-layout')
  const form = page.locator('form')
  await expect(form.getByLabel('Name', { exact: true })).toHaveValue('Layout client')
  await expect(page.getByRole('heading', { level: 1 }).filter({ hasText: 'Layout client' })).toBeVisible()
  const pageOrder = await editorControlIds(form)
  const box = async (selector: string) => {
    const rect = await page.locator(selector).boundingBox()
    if (!rect) throw new Error(`${selector} is not visible`)
    return rect
  }
  const rem = await page.evaluate(() => parseFloat(getComputedStyle(document.documentElement).fontSize))
  // The RPM control is the 7rem input group including its unit suffix.
  const rpmGroup = await box('[data-slot="input-group"]:has(#api-key-rpm-limit)')
  expect(Math.abs(rpmGroup.width - 7 * rem)).toBeLessThanOrEqual(1)
  expect((await box('#api-key-name')).width).toBeLessThanOrEqual(24 * rem + 1)
  expect((await box('#api-key-expires')).width).toBeLessThanOrEqual(18 * rem + 1)
  expect((await box('#api-key-secret')).width).toBeLessThanOrEqual(36 * rem + 1)

  // Switches are rows, never side by side; auto-exposed features sit indented under their parent row.
  const parent = await box('#api-key-transparent-injection')
  const mcp = await box('#api-key-mcp-access')
  const child = await box('#api-key-inject-web-search')
  expect(parent.y).toBeGreaterThan(mcp.y + mcp.height)
  expect(child.y).toBeGreaterThan(parent.y + parent.height)
  expect(Math.abs(child.x + child.width - (parent.x + parent.width))).toBeLessThan(8)

  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/api-keys')
  await page.getByRole('row').filter({ hasText: 'Layout client' }).getByRole('cell').nth(1).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await expect(sheet.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()
  // The configuration summary that used to repeat under every title is gone.
  await expect(sheet.getByText(/Set RPM, expiry, model access/)).toHaveCount(0)
  expect(await editorControlIds(sheet)).toEqual(pageOrder)
})

test('API Key Sheet keeps switch rows readable at 390px', async ({ page }) => {
  await mockLayoutKey(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/api-keys')
  await page
    .getByRole('button', { name: /Layout client/ })
    .first()
    .click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await expect(sheet.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()

  const ids = [
    'api-key-enabled',
    'api-key-allow-all-models',
    'api-key-mcp-access',
    'api-key-transparent-injection',
    'api-key-inject-web-search',
  ]
  const rects = []
  for (const id of ids) {
    await page.locator(`#${id}`).scrollIntoViewIfNeeded()
    const rect = await page.locator(`#${id}`).boundingBox()
    if (!rect) throw new Error(`${id} is not visible`)
    rects.push(rect)
  }
  const sheetBox = await sheet.boundingBox()
  if (!sheetBox) throw new Error('Sheet is not visible')
  for (const [index, rect] of rects.entries()) {
    expect(rect.x + rect.width).toBeLessThanOrEqual(sheetBox.x + sheetBox.width)
    if (index > 0) expect(rect.y - rects[index - 1].y).toBeGreaterThanOrEqual(44)
  }
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})

test('API Key direct page locks editing and leaving while saving and keeps the draft on failure', async ({ page }) => {
  const { promise: gate, resolve: release } = Promise.withResolvers<void>()
  await mockLayoutKey(page, async (route) => {
    await gate
    await route.fulfill({ status: 500, json: { error: { message: 'Save rejected', code: 'internal_error' } } })
  })
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/api-keys/key-layout')
  const name = page.getByLabel('Name', { exact: true })
  await name.fill('Renamed on page')
  await page.getByRole('button', { name: 'Save API Key' }).click()

  await expect(name).toBeDisabled()
  await expect(page.locator('#api-key-mcp-access')).toBeDisabled()
  await expect(page.getByRole('button', { name: 'Save API Key' })).toBeDisabled()
  await expect(page.getByRole('link', { name: 'Cancel' })).toHaveAttribute('aria-disabled', 'true')
  await page.locator('a[href="/providers"]').first().click()
  await expect(page).toHaveURL(/\/api-keys\/key-layout$/)
  await expect(page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })).toHaveCount(0)

  release()
  await expect(name).toBeEnabled()
  await expect(name).toHaveValue('Renamed on page')
  await expect(page.locator('[data-sonner-toast]')).toBeVisible()
  // The draft survived, so leaving now asks first.
  await page.locator('a[href="/providers"]').first().click()
  await expect(page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })).toBeVisible()
})
