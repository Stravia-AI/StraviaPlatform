import { expect, test, type Page } from '@playwright/test'
import type { ApiKey, Route } from '../src/lib/types'
import { prepareApp } from './prepare-app'

const model: Route = {
  id: 'route-one',
  model_id: 'upstream-exact',
  display_name: 'My model',
  balance: 'traffic_equalization',
  output_modality: 'text',
  image_generation_bound: false,
  target_provider: 'provider-one',
  target_model: 'upstream-exact',
  is_enabled: true,
  created_at: '2026-01-01T00:00:00Z',
  supported_thinking_levels: [],
  targets: [],
}
function key(id: string, overrides: Partial<ApiKey> = {}): ApiKey {
  return {
    id,
    name: id,
    key: `sk-isolated-${id}`,
    rpm_limit: null,
    is_enabled: true,
    mcp_access_enabled: false,
    transparent_injection_enabled: false,
    inject_web_search: false,
    inject_media_understanding: false,
    expires_at: null,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
    model_ids: [],
    ...overrides,
  }
}
async function setup(page: Page, keys: ApiKey[]): Promise<void> {
  await prepareApp(page)
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [model] } }))
  await page.route('**/api/v1/api-keys', (route) => route.fulfill({ json: { data: keys } }))
  await page.route('**/api/v1/connect-clients/preview', (route) =>
    route.fulfill({
      json: { data: { paths: [], preview: '[model_providers.stravia]\nbase_url = "http://localhost/v1"' } },
    }),
  )
}

test('Connect selects the sole eligible key, excluding disabled, expired and inaccessible credentials', async ({
  page,
}) => {
  await setup(page, [
    key('Disabled', { is_enabled: false }),
    key('Expired', { expires_at: '2000-01-01T00:00:00Z' }),
    key('Restricted', { model_ids: ['other-route'] }),
    key('Personal'),
  ])
  await page.goto('/connect')
  await expect(page.locator('#cli-key')).toContainText('Personal')
  await page.locator('#cli-key').click()
  await expect(page.getByRole('option', { name: /Personal/ })).toBeVisible()
  for (const name of ['Disabled', 'Expired', 'Restricted']) {
    await expect(page.getByRole('option', { name: new RegExp(name) })).toHaveCount(0)
  }
  await page.keyboard.press('Escape')
  await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeEnabled()
  await expect(page.getByRole('button', { name: 'Write configuration', exact: true })).toHaveCount(0)
})

test('Connect keeps an explicit key and client selection after managing credentials', async ({ page }) => {
  await setup(page, [key('Personal'), key('Separate')])
  await page.goto('/connect')
  await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeDisabled()
  await page.locator('#cli-key').click()
  await page.getByRole('option', { name: /Separate/ }).click()
  await page.locator('#cli-tool').click()
  await page.getByRole('option', { name: 'OpenCode', exact: true }).click()
  await page.getByRole('button', { name: 'Manage API Keys', exact: true }).click()
  await expect(page).toHaveURL(/\/api-keys$/)
  await page.getByRole('link', { name: 'Continue setup', exact: true }).click()
  await expect(page.locator('#cli-tool')).toContainText('OpenCode')
  await expect(page.locator('#cli-key')).toContainText('Separate')
  await page
    .getByRole('navigation', { name: 'Primary navigation' })
    .getByRole('link', { name: 'API Keys', exact: true })
    .click()
  await expect(page.getByRole('link', { name: 'Continue setup', exact: true })).toHaveCount(0)
})

test('Connect exposes recovery without eligible keys and retains code choices on return', async ({ page }) => {
  const keys = [key('Disabled', { is_enabled: false })]
  await setup(page, keys)
  await page.goto('/connect')
  await expect(page.locator('#cli-key')).toBeDisabled()
  await expect(page.getByText('No eligible API Key')).toBeVisible()
  await page.getByRole('tab', { name: 'Code', exact: true }).click()
  await page.locator('#code-protocol').click()
  await page.getByRole('option', { name: 'Open Responses', exact: true }).click()
  await page.locator('#code-model').click()
  await page.getByRole('option', { name: /My model/ }).click()
  await page.getByRole('tab', { name: 'cURL', exact: true }).click()
  await page.getByRole('button', { name: 'Manage API Keys', exact: true }).click()
  keys.push(key('New key'))
  await page.getByRole('link', { name: 'Continue setup', exact: true }).click()
  await expect(page.getByRole('tab', { name: 'Code', exact: true })).toHaveAttribute('aria-selected', 'true')
  await expect(page.locator('#code-protocol')).toContainText('Open Responses')
  await expect(page.locator('#code-model')).toContainText('My model')
  await expect(page.locator('#code-key')).toContainText('New key')
  await expect(page.getByRole('tab', { name: 'cURL', exact: true })).toHaveAttribute('aria-selected', 'true')
  await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeEnabled()
})

test('Copy failure never reports success and leaves the configuration available', async ({ page }) => {
  await setup(page, [key('Personal')])
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', { value: { writeText: () => Promise.reject(new Error('denied')) } })
  })
  await page.goto('/connect')
  await page.getByRole('button', { name: 'Copy', exact: true }).click()
  await expect(page.getByText('Could not copy to clipboard')).toBeVisible()
  await expect(page.getByText('Copied to clipboard', { exact: true })).toHaveCount(0)
  await expect(page.locator('pre')).toBeVisible()
})

test('Connect config preview fills the available page height', async ({ page }) => {
  await page.setViewportSize({ width: 1568, height: 853 })
  await setup(page, [key('Personal')])
  await page.goto('/connect')
  const cliPreview = page.getByRole('tabpanel').locator('pre.route-code-plane')
  await expect(cliPreview).toBeVisible()
  // 高窗口必须越过旧的 32rem 上限，短配置也应填满剩余空间。
  for (const height of [853, 1384]) {
    await page.setViewportSize({ width: 1568, height })
    await expect
      .poll(async () => {
        const mainBox = (await page.getByRole('main').boundingBox())!
        const previewBox = (await cliPreview.boundingBox())!
        return mainBox.y + mainBox.height - previewBox.y - previewBox.height
      })
      .toBeLessThanOrEqual(24)
  }

  await page.getByRole('tab', { name: 'Code', exact: true }).click()
  await page.locator('#code-model').click()
  await page.getByRole('option', { name: /My model/ }).click()
  const codePreview = page.getByRole('tabpanel').locator('pre.route-code-plane')
  await expect(codePreview).toBeVisible()
  const codeBox = await codePreview.boundingBox()
  expect(codeBox!.y + codeBox!.height).toBeGreaterThan(1384 - 80)

  await page.setViewportSize({ width: 800, height: 900 })
  await expect.poll(async () => (await codePreview.boundingBox())!.height).toBeGreaterThanOrEqual(384)
  await page.getByRole('tab', { name: 'Clients', exact: true }).click()
  await expect.poll(async () => (await cliPreview.boundingBox())!.height).toBeGreaterThanOrEqual(384)
  await cliPreview.scrollIntoViewIfNeeded()
  await expect(cliPreview).toBeInViewport()
})

test('Connect shrinks the config preview on short windows instead of scrolling the page', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 640 })
  await setup(page, [key('Personal')])
  await page.route('**/api/v1/connect-clients/preview', (route) =>
    route.fulfill({
      json: {
        data: { paths: [], preview: Array.from({ length: 120 }, (_, index) => `line_${index} = "value"`).join('\n') },
      },
    }),
  )
  await page.goto('/connect')
  const preview = page.locator('pre.route-code-plane')
  await expect(preview).toBeVisible()
  const main = page.locator('main')
  await expect.poll(async () => main.evaluate((el) => el.scrollHeight - el.clientHeight)).toBeLessThanOrEqual(1)
  const previewBox = await preview.evaluate((el) => ({ scroll: el.scrollHeight, client: el.clientHeight }))
  expect(previewBox.scroll).toBeGreaterThan(previewBox.client)
  expect(previewBox.client).toBeLessThan(512)
})

test('Claude requires all four mappings while other clients do not ask for a default model', async ({ page }) => {
  await setup(page, [key('Personal')])
  await page.goto('/connect')
  await expect(page.locator('#cli-default-model')).toHaveCount(0)
  await page.locator('#cli-tool').click()
  await page.getByRole('option', { name: 'Claude Code', exact: true }).click()
  for (const id of ['cli-default-model', 'cli-haiku-model', 'cli-sonnet-model', 'cli-opus-model']) {
    await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeDisabled()
    await page.locator(`#${id}`).click()
    await page.getByRole('option', { name: /My model/ }).click()
  }
  await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeEnabled()
})

test('Wide workspace stacks labels over bounded controls sharing one column, including Claude mappings', async ({
  page,
}) => {
  await page.setViewportSize({ width: 2560, height: 1440 })
  await setup(page, [key('Personal')])
  await page.goto('/connect')
  await page.locator('#cli-tool').click()
  await page.getByRole('option', { name: 'Claude Code', exact: true }).click()
  const ids = ['cli-tool', 'cli-key', 'cli-default-model', 'cli-haiku-model', 'cli-sonnet-model', 'cli-opus-model']
  await expect(page.locator('#cli-opus-model')).toBeVisible()
  const workspace = (await page.locator('.connect-page').boundingBox())!
  expect(Math.abs(workspace.width - 1920)).toBeLessThanOrEqual(1)
  const boxes = []
  for (const id of ids) boxes.push((await page.locator(`#${id}`).boundingBox())!)
  const right = boxes[0].x + boxes[0].width
  for (const box of boxes) {
    expect(box.width).toBeGreaterThanOrEqual(18 * 16 - 0.5)
    expect(box.width).toBeLessThanOrEqual(24 * 16 + 0.5)
    expect(Math.abs(box.x - boxes[0].x)).toBeLessThanOrEqual(0.5)
    expect(Math.abs(box.x + box.width - right)).toBeLessThanOrEqual(0.5)
  }
  // 标签在控件上方。
  const label = (await page.locator('label[for="cli-key"]').boundingBox())!
  expect(label.y + label.height).toBeLessThanOrEqual(boxes[1].y)
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
})

test('Long selected values truncate inside the bounded select and keep the chevron visible', async ({ page }) => {
  const longName = 'Production-automation-credential-for-the-nightly-release-and-rollback-pipeline'
  await setup(page, [key(longName)])
  await page.setViewportSize({ width: 2560, height: 1440 })
  await page.goto('/connect')
  await page.getByRole('tab', { name: 'Code', exact: true }).click()
  await page.locator('#code-model').click()
  await page.getByRole('option', { name: /My model/ }).click()
  await expect(page.locator('#code-key')).toContainText(longName)
  for (const id of ['code-protocol', 'code-model', 'code-key']) {
    const trigger = page.locator(`#${id}`)
    await expect(trigger).toBeVisible()
    const overflow = await trigger.evaluate((el) => el.scrollWidth - el.clientWidth)
    expect(overflow, `${id} content must not overflow its trigger`).toBeLessThanOrEqual(0)
  }
  const truncated = await page.locator('#code-key > span').evaluate((el) => el.scrollWidth > el.clientWidth)
  expect(truncated).toBe(true)
})

test('Narrow control column stacks label above a bounded select and splits only at workspace width', async ({
  page,
}) => {
  await setup(page, [key('Personal')])
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/connect')
  const select = page.locator('#cli-key')
  await expect(select).toBeVisible()
  const label = (await page.locator('label[for="cli-key"]').boundingBox())!
  const stacked = (await select.boundingBox())!
  expect(label.y + label.height).toBeLessThanOrEqual(stacked.y)
  expect(stacked.width).toBeGreaterThan(256)
  expect(stacked.width).toBeLessThanOrEqual(448 + 0.5)
  // 两栏：控制列在左、预览在右。
  const preview = (await page.locator('pre.route-code-plane').boundingBox())!
  expect(preview.x).toBeGreaterThan(stacked.x + stacked.width)
  // 工作区不足 68rem：单栏，预览在控件之下，页面不横向溢出。
  await page.setViewportSize({ width: 1100, height: 900 })
  await expect
    .poll(async () => (await page.locator('pre.route-code-plane').boundingBox())!.x)
    .toBeLessThan((await select.boundingBox())!.x + 1)
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
})

test('Optional explanations live in help tooltips and no descriptive copy repeats the header or client', async ({
  page,
}) => {
  await setup(page, [key('Personal')])
  await page.goto('/connect')
  await expect(page.locator('pre.route-code-plane')).toBeVisible()
  for (const text of [
    'OpenAI coding CLI tool',
    'All models available to this API Key will be included in the client configuration.',
  ]) {
    await expect(page.getByText(text)).toHaveCount(0)
  }
  await page.getByRole('button', { name: 'More about this field' }).first().focus()
  await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('OpenAI coding CLI tool')
  // 已选可用密钥：不出现阻碍原因，创建仍是可达的次要入口。
  await expect(page.getByText('No eligible API Key')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Create an API Key', exact: true })).toBeVisible()
})

test('Choosing client and key is local view state and never sends a save request', async ({ page }) => {
  const writes: string[] = []
  page.on('request', (request) => {
    const url = new URL(request.url())
    if (
      request.method() !== 'GET' &&
      url.pathname.startsWith('/api/') &&
      !url.pathname.endsWith('/connect-clients/preview') &&
      !url.pathname.endsWith('/updates/check')
    )
      writes.push(`${request.method()} ${url.pathname}`)
  })
  await setup(page, [key('Personal'), key('Separate')])
  await page.goto('/connect')
  await page.locator('#cli-key').click()
  await page.getByRole('option', { name: /Separate/ }).click()
  await page.locator('#cli-tool').click()
  await page.getByRole('option', { name: 'OpenCode', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeEnabled()
  await expect(page.getByRole('button', { name: /^(Save|Cancel)/ })).toHaveCount(0)
  expect(writes).toEqual([])
})

test('On a phone the preview starts earlier than the old layout and keeps its minimum height', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await setup(page, [key('Personal')])
  await page.goto('/connect')
  const preview = page.locator('pre.route-code-plane')
  await expect(preview).toBeVisible()
  const box = await preview.evaluate((el) => {
    const rect = el.getBoundingClientRect()
    return { top: rect.top + window.scrollY, height: rect.height }
  })
  // 旧布局预览起于 y≈705（含眉题、描述与工具说明）。
  expect(box.top).toBeLessThan(705)
  expect(box.height).toBeGreaterThanOrEqual(384)
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
})
