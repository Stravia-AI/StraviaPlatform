import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('API Key editor preserves disabled transparent injection selections', async ({ page }) => {
  let persistedKey = {
    id: 'key-advanced',
    key: 'sk-advanced',
    name: 'Advanced client',
    concurrency_limit: null,
    is_enabled: true,
    mcp_access_enabled: true,
    transparent_injection_enabled: true,
    inject_media_understanding: true,
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
    await route.fulfill({ json: { data: { enabled: false } } })
  })

  await page.setViewportSize({ width: 1279, height: 800 })
  await page.goto('/api-keys')
  const tableContainer = page.locator('[data-slot="table-container"]')
  await expect(tableContainer).toBeVisible()
  expect(await tableContainer.evaluate((element) => element.scrollWidth)).toBeLessThanOrEqual(
    await tableContainer.evaluate((element) => element.clientWidth),
  )
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveCount(0)
  await page.getByRole('row').filter({ hasText: 'Advanced client' }).getByRole('cell').nth(1).click()
  const editor = page.locator('[data-slot="sheet-content"]')
  await expect(editor.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()
  await expect(page).toHaveURL(/\/api-keys$/)
  await expect(editor.getByRole('button', { name: 'Advanced', exact: true })).toHaveCount(0)
  await expect(page.locator('#api-key-mcp-access')).toHaveAttribute('aria-checked', 'true')
  await expect(page.locator('#api-key-transparent-injection')).toHaveAttribute('aria-checked', 'true')
  const mediaSelection = page.locator('#api-key-inject-media-understanding')
  const searchSelection = page.locator('#api-key-inject-web-search')
  await expect(mediaSelection).toHaveAttribute('aria-checked', 'true')
  await expect(searchSelection).toHaveAttribute('aria-checked', 'true')
  await expect(mediaSelection).toBeDisabled()
  await expect(searchSelection).toBeDisabled()
  await expect(
    page.getByRole('link', { name: 'Turn on this feature in Advanced Features before Stravia can expose it.' }),
  ).toHaveCount(2)
  await page.getByRole('button', { name: 'Save API Key' }).click()

  expect(persistedKey.inject_media_understanding).toBe(true)
  expect(persistedKey.inject_web_search).toBe(true)
  await expect(editor).toBeHidden()
  await expect(page).toHaveURL(/\/api-keys$/)
  await page.reload()
  await page.getByRole('row').filter({ hasText: 'Advanced client' }).getByRole('cell').nth(2).click()
  await expect(page.locator('#api-key-inject-media-understanding')).toHaveAttribute('aria-checked', 'true')
  await expect(page.locator('#api-key-inject-web-search')).toHaveAttribute('aria-checked', 'true')
})

test('API Key editor persists concurrency and Model Route selections', async ({ page }) => {
  const modelId = '4d93a8ac-0d1f-4891-8780-55c2f566c084'
  const secondModelId = 'd414f418-547e-4269-b917-fec9a10c39bb'
  const modelName = 'gpt-5.6-sol'
  const secondModelName = 'gpt-5.6-luna'
  let modelIds: string[] = []
  let concurrencyLimit: number | null = null
  const apiKey = {
    id: 'key-test',
    key: 'sk-test-secret',
    name: 'test',
    concurrency_limit: null,
    is_enabled: true,
    mcp_access_enabled: true,
    transparent_injection_enabled: true,
    inject_media_understanding: true,
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
    await route.fulfill({ json: { data: [{ ...apiKey, concurrency_limit: concurrencyLimit, model_ids: modelIds }] } })
  })
  await page.route('**/api/v1/api-keys/key-test', async (route) => {
    const input = route.request().postDataJSON()
    modelIds = input.model_ids
    concurrencyLimit = input.concurrency_limit
    await route.fulfill({ json: { data: { ...apiKey, concurrency_limit: concurrencyLimit, model_ids: modelIds } } })
  })

  await page.goto('/api-keys')
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveCount(0)
  await page.getByRole('row').filter({ hasText: 'test' }).getByRole('cell').nth(2).click()
  const editor = page.locator('[data-slot="sheet-content"]')
  await expect(editor.getByRole('heading', { name: 'Edit API Key' })).toBeVisible()
  await expect(page).toHaveURL(/\/api-keys$/)
  await page.setViewportSize({ width: 493, height: 832 })
  const center = async (selector: string) => {
    const box = await page.locator(selector).boundingBox()
    if (!box) throw new Error(`${selector} is not visible`)
    return { x: box.x + box.width / 2, y: box.y + box.height / 2 }
  }
  const [nameCenter, enabledCenter] = await Promise.all([center('#api-key-name'), center('#api-key-enabled')])
  expect(Math.abs(nameCenter.y - enabledCenter.y)).toBeLessThan(1)
  await page.setViewportSize({ width: 544, height: 832 })
  const allowAllModels = page.getByRole('switch', { name: 'Allow all models' })
  await expect(allowAllModels).toBeChecked()
  await expect(page.getByRole('combobox', { name: 'Select allowed models' })).toHaveCount(0)
  await allowAllModels.click()
  await expect(allowAllModels).not.toBeChecked()
  const modelPicker = page.locator('#api-key-model-picker')
  await expect(modelPicker).toContainText('Allowed: 2')
  const [allowAllCenter, modelPickerCenter] = await Promise.all([
    center('#api-key-allow-all-models'),
    center('#api-key-model-picker'),
  ])
  expect(Math.abs(allowAllCenter.y - modelPickerCenter.y)).toBeLessThan(1)
  expect(allowAllCenter.x).not.toBe(modelPickerCenter.x)
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
  await expect(modelMenu.locator('[data-slot="command-separator"]')).toHaveCount(1)
  await page.keyboard.press('Escape')
  await page.setViewportSize({ width: 493, height: 832 })
  await expect(editor.getByRole('button', { name: 'Advanced', exact: true })).toHaveCount(0)
  const [concurrencyCenter, expiryCenter, mcpCenter, transparentCenter, mediaCenter, webSearchCenter] =
    await Promise.all([
      center('#api-key-concurrency-limit'),
      center('#api-key-expires'),
      center('#api-key-mcp-access'),
      center('#api-key-transparent-injection'),
      center('#api-key-inject-media-understanding'),
      center('#api-key-inject-web-search'),
    ])
  expect(Math.abs(concurrencyCenter.y - expiryCenter.y)).toBeLessThan(1)
  expect(Math.abs(mcpCenter.y - transparentCenter.y)).toBe(40)
  expect(Math.abs(mediaCenter.y - webSearchCenter.y)).toBeLessThan(1)
  expect(mediaCenter.x).not.toBe(webSearchCenter.x)

  await expect(page.getByLabel('Maximum concurrent executions')).toHaveValue('')
  await page.getByLabel('Maximum concurrent executions').fill('2')
  await page.getByRole('button', { name: 'Save API Key' }).click()
  await expect(page.getByText('API key saved.')).toBeVisible()
  await expect.poll(() => modelIds).toEqual([modelId])
  await expect.poll(() => concurrencyLimit).toBe(2)
  await expect(editor).toBeHidden()
  await page.setViewportSize({ width: 1280, height: 720 })
  await expect(page.getByRole('cell', { name: 'Concurrent executions 2' })).toBeVisible()

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
