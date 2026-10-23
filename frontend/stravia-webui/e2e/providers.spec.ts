import { expect, test, type Page } from '@playwright/test'

import type { ProviderMetadata } from '../src/lib/types'
import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

for (const locale of ['en-US', 'zh-CN']) {
  test(`shows independent credential and allowance evidence in list and detail (${locale})`, async ({ page }) => {
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    const provider = {
      id: 'paused-provider',
      name: 'Paused account',
      vendor: 'openai',
      channel: 'default',
      protocol: 'openai-compatible',
      base_url: 'https://fixture.invalid/v1',
      use_proxy: false,
      configured_credential_fields: ['api_key'],
      is_enabled: true,
      credential_status: 'invalid',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
      allowance_suspension: {
        suspended_at: '2026-09-01T12:00:00Z',
        triggered_keys: ['weekly'],
        earliest_reset_at: 1788883200000,
      },
    }
    await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
    await page.route('**/api/v1/providers/paused-provider', (route) => route.fulfill({ json: { data: provider } }))
    await page.route('**/api/v1/providers/paused-provider/models', (route) =>
      route.fulfill({ json: { data: { models: [] } } }),
    )
    await page.goto('/providers')
    await expect(
      page
        .getByRole('status')
        .filter({ hasText: locale === 'en-US' ? 'Credential invalid' : '凭据失效' })
        .first(),
    ).toBeVisible()
    const pauseLabel = locale === 'en-US' ? 'Allowance paused' : '额度暂停'
    const pause = page.getByRole('button', { name: pauseLabel, exact: true }).first()
    await expect(pause).toBeVisible()
    const row = page.getByRole('row').filter({ has: pause })
    const height = (await row.boundingBox())!.height
    expect(height).toBeLessThan(100)
    await pause.hover()
    await expect(page.getByRole('tooltip')).toContainText('weekly')
    expect((await row.boundingBox())!.height).toBe(height)
    await pause.press('Escape')
    await expect(page.getByRole('tooltip')).not.toBeVisible()
    await pause.press('Tab')
    await pause.focus()
    await expect(page.getByRole('tooltip')).toContainText('weekly')
    await page.goto('/providers/paused-provider?view=connection')
    await expect(
      page
        .getByRole('status')
        .filter({ hasText: locale === 'en-US' ? 'Credential invalid' : '凭据失效' })
        .first(),
    ).toBeVisible()
    await page.getByRole('button', { name: pauseLabel, exact: true }).focus()
    await expect(page.getByRole('tooltip')).toContainText('weekly')
    await page.setViewportSize({ width: 320, height: 850 })
    await page.goto('/providers')
    const mobilePause = page.getByRole('button', { name: pauseLabel, exact: true })
    await mobilePause.click()
    await expect(page).toHaveURL(/\/providers$/)
    const tooltip = page.getByRole('tooltip')
    await expect(tooltip).toContainText('weekly')
    const bounds = (await tooltip.boundingBox())!
    expect(bounds.x).toBeGreaterThanOrEqual(0)
    expect(bounds.x + bounds.width).toBeLessThanOrEqual(320)
    await mobilePause.press('Escape')
    await expect(tooltip).not.toBeVisible()
    await mobilePause.click()
    await expect(tooltip).toContainText('weekly')
  })
}

test('Provider detail header names the provider, keeps status on the title row, and keeps recovery when missing', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  const provider = {
    id: 'unnamed-provider',
    name: '',
    vendor: 'openai',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://fixture.invalid/v1',
    use_proxy: false,
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    credential_status: 'invalid',
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
  await page.route('**/api/v1/providers/unnamed-provider/models', (route) =>
    route.fulfill({ json: { data: { models: [] } } }),
  )

  await page.goto('/providers/unnamed-provider?view=connection')
  const title = page.getByRole('heading', { level: 1 })
  await expect(title).toHaveText('unnamed-provider')
  const header = page.locator('header').filter({ has: title })
  await expect(header.locator('p')).toHaveCount(0)
  const status = header.getByRole('status').filter({ hasText: 'Credential invalid' })
  await expect(status).toBeVisible()
  const titleBox = (await title.boundingBox())!
  const statusBox = (await status.boundingBox())!
  expect(statusBox.x).toBeGreaterThanOrEqual(titleBox.x + titleBox.width)
  expect(Math.abs(statusBox.y + statusBox.height / 2 - (titleBox.y + titleBox.height / 2))).toBeLessThan(
    titleBox.height / 2,
  )

  await page.goto('/providers/missing-provider')
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Provider not found')
  await expect(page.getByRole('main').getByText('This saved provider is no longer available.')).toHaveCount(0)
  await expect(page.getByRole('link', { name: 'Back to providers' })).toHaveAttribute('href', '/providers')
})

test('service cards filter sign-in methods and use local logos before remote fallback', async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('stravia-locale', 'zh-CN'))
  let failLogo = false
  let localLogoRequests = 0
  let remoteLogoRequests = 0
  await page.route('**/api/v1/catalog/providers/openai/logo', async (route) => {
    localLogoRequests += 1
    await route.fulfill({ status: 404, body: '' })
  })
  await page.route('**/api/v1/catalog/providers/openai-compatible/logo', async (route) => {
    remoteLogoRequests += 1
    if (failLogo) {
      await route.fulfill({ status: 404, body: '' })
    } else {
      await route.fulfill({
        contentType: 'image/svg+xml',
        body: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1 1"><path d="M0 0h1v1H0z"/></svg>',
      })
    }
  })
  await page.goto('/providers')
  await page.getByRole('button', { name: /添加.*供应商/ }).click()
  const grid = page.locator('[data-provider-grid]')
  const openAi = grid.getByRole('button', { name: /^OpenAI API 密钥 / })
  const compatible = grid.getByRole('button', { name: /^OpenAI Compatible API 密钥 / })
  await expect(openAi.locator('img')).toHaveAttribute('src', /^data:image\/svg\+xml,/)
  await expect
    .poll(() => openAi.locator('img').evaluate((img: HTMLImageElement) => img.naturalWidth))
    .toBeGreaterThan(0)
  await expect(compatible.locator('img')).toHaveAttribute('src', /\/catalog\/providers\/openai-compatible\/logo$/)
  await expect
    .poll(() => compatible.locator('img').evaluate((img: HTMLImageElement) => img.naturalWidth))
    .toBeGreaterThan(0)
  expect(remoteLogoRequests).toBeGreaterThan(0)
  expect(localLogoRequests).toBe(0)

  const search = page.locator('[data-provider-toolbar] input')
  await search.fill('账号')
  await expect(grid.getByRole('button')).toHaveCount(1)
  await expect(grid.getByRole('button', { name: /Codex · OAuth 账号/ })).toBeVisible()
  await search.fill('API 密钥')
  await expect(openAi).toBeVisible()
  await expect(grid.getByRole('button', { name: /OAuth 账号/ })).toHaveCount(0)

  failLogo = true
  const successfulLogoRequests = remoteLogoRequests
  await page.reload()
  await page.getByRole('button', { name: /添加.*供应商/ }).click()
  await expect.poll(() => remoteLogoRequests).toBeGreaterThan(successfulLogoRequests)
  await expect(compatible.locator('img')).toHaveCount(0)
  await expect(compatible.locator('.route-provider-mark')).toHaveText('O')
  await expect(compatible.locator('.route-provider-mark')).toBeVisible()
  await expect(openAi.locator('img')).toHaveAttribute('src', /^data:image\/svg\+xml,/)
  await expect
    .poll(() => openAi.locator('img').evaluate((img: HTMLImageElement) => img.naturalWidth))
    .toBeGreaterThan(0)
  expect(localLogoRequests).toBe(0)
})

test('model ID suggestions preserve arbitrary text, composition, and form focus', async ({ page }) => {
  await page.goto('/models/new')
  const modelId = page.locator('#route-model-id')
  const displayName = page.locator('#route-display-name')
  await displayName.fill('Unsaved display name')
  await modelId.fill('private/自定义-model')
  await modelId.press('Enter')
  await expect(modelId).toHaveValue('private/自定义-model')
  await expect(displayName).toHaveValue('Unsaved display name')
  await expect(page).toHaveURL(/\/models\/new$/)

  await modelId.fill('gpt-5.4')
  await expect(page.getByRole('option', { name: /GPT-5.4/ })).toBeVisible()
  await modelId.dispatchEvent('compositionstart')
  await modelId.dispatchEvent('keydown', { key: 'Enter', code: 'Enter', isComposing: true, bubbles: true })
  await expect(modelId).toHaveValue('gpt-5.4')
  await expect(displayName).toHaveValue('Unsaved display name')
  await expect(page.getByRole('option', { name: /GPT-5.4/ })).toBeVisible()
  await modelId.dispatchEvent('compositionend')
  await modelId.press('ArrowDown')
  await modelId.press('Enter')
  await expect(modelId).toHaveValue('gpt-5.4')
  await expect(displayName).toHaveValue('GPT-5.4')
  await expect(modelId).toBeFocused()

  await modelId.fill('claude')
  await modelId.press('Escape')
  await expect(page.getByRole('option', { name: /Claude Opus/ })).not.toBeVisible()
  await expect(modelId).toHaveValue('claude')
  await modelId.press('Tab')
  await expect(page.getByRole('button', { name: 'Clear selected model', exact: true })).toBeFocused()
  await page.keyboard.press('Tab')
  await expect(displayName).toBeFocused()
  await modelId.focus()
  await expect(page.getByRole('option', { name: /Claude Opus/ })).toBeVisible()
  await page.getByRole('button', { name: 'Clear selected model', exact: true }).click()
  await expect(modelId).toHaveValue('')
  await expect(displayName).toHaveValue('')
  await expect(modelId).toBeFocused()
})

test('configured Providers table filters and persists column customization', async ({ page }) => {
  const configuredProviders = [
    {
      id: 'zeta-provider',
      name: 'Zeta Service',
      vendor: 'openai',
      protocol: 'openai-compatible',
      base_url: 'https://zeta.example/v1',
      use_proxy: false,
      channel: 'default',
      configured_credential_fields: ['api_key'],
      is_enabled: true,
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    },
    {
      id: 'alpha-provider',
      name: 'Alpha Service',
      vendor: 'openai-codex',
      protocol: 'open-responses',
      base_url: 'https://alpha.example/v1',
      use_proxy: false,
      channel: 'codex',
      configured_credential_fields: [],
      is_enabled: false,
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    },
  ]
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({ json: { data: configuredProviders } })
  })

  await page.setViewportSize({ width: 1279, height: 800 })
  await page.goto('/providers')

  const table = page.getByRole('table', { name: 'Connected AI providers' })
  await expect(table.getByRole('row')).toHaveCount(3)
  // 窄屏列表有自己的搜索框（桌面宽度下隐藏），按可访问树取可见的那一个。
  const search = page.getByRole('textbox', { name: 'Search providers…' })
  await search.fill('Alpha')
  await expect(table.getByRole('row')).toHaveCount(2)
  await expect(table).toContainText('Alpha Service')
  await expect(table).not.toContainText('Zeta Service')
  await search.fill('__missing__')
  await expect(table).toContainText('No providers match your search.')
  await search.fill('')

  await page.getByRole('button', { name: 'Columns' }).click()
  await page.getByRole('menuitemcheckbox', { name: 'Authentication' }).click()
  await expect(table.getByRole('columnheader', { name: /Authentication/ })).toHaveCount(0)

  await page.reload()
  await expect(table.getByRole('columnheader', { name: /Authentication/ })).toHaveCount(0)
  await table.getByRole('button', { name: 'Sort Provider ascending' }).click()
  await expect(table.getByRole('row').nth(1)).toContainText('Alpha Service')
})

test('provider editor selects a service before validating its configuration', async ({ page }) => {
  let previewAttempts = 0
  await page.route('**/api/v1/providers/configuration-preview', async (route) => {
    previewAttempts += 1
    const input = route.request().postDataJSON() as { base_url: string }
    await route.fulfill({
      json: {
        data: {
          base_url: input.base_url,
          issues:
            previewAttempts === 1
              ? [
                  {
                    field: 'apiKey',
                    code: 'credential_rejected',
                    message: { 'en-US': 'Use a valid fixture credential.', 'zh-CN': '请使用有效的测试凭据。' },
                  },
                ]
              : [],
          network_permissions: [],
        },
      },
    })
  })
  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()

  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Connect', exact: true })).toHaveCount(0)
  const searchInput = page.locator('[data-provider-toolbar] input')
  await expect(searchInput).toBeVisible()
  const refreshButton = page.getByRole('button', { name: 'Reload providers' })
  await expect(refreshButton).toBeVisible()
  await refreshButton.click()
  await expect(page.getByRole('button', { name: /^OpenAI Compatible / })).toBeVisible()
  await expect(page.getByRole('button', { name: /^OpenAI API key / })).toBeVisible()
  await expect(page.getByRole('button', { name: /OpenAI.*Codex.*OAuth account/ })).toBeVisible()

  await searchInput.fill('OpenAI')
  await expect(page.getByRole('button', { name: /^OpenAI API key / })).toBeVisible()
  await expect(page.getByRole('button', { name: /OpenAI.*Codex.*OAuth account/ })).toBeVisible()
  await expect(page.getByRole('tab', { name: 'Choose provider' })).toHaveAttribute('aria-selected', 'true')
  const openAiOption = page.getByRole('button', { name: /^OpenAI API key / })
  const codexOption = page.getByRole('button', { name: /OpenAI.*Codex.*OAuth account/ })
  await openAiOption.focus()
  await openAiOption.press('ArrowRight')
  await expect(codexOption).toBeFocused()

  await searchInput.fill('__missing_provider__')
  await expect(page.getByText('No matching providers', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Clear search' }).click()
  await expect(page.getByText('No matching providers', { exact: true })).toHaveCount(0)

  const compatibleOption = page.getByRole('button', { name: /^OpenAI Compatible / })
  await compatibleOption.click()

  await expect(page.getByRole('heading', { name: 'Connection details' })).toBeVisible()
  const providerForm = page.locator('form.route-overlay-form')
  const baseUrl = page.getByLabel('Base URL')
  const connect = page.getByRole('button', { name: 'Connect', exact: true })
  const apiKey = page.getByLabel('API key', { exact: true })
  await expect.poll(() => providerForm.evaluate((form: HTMLFormElement) => form.checkValidity())).toBe(false)
  await baseUrl.fill('https://custom.example/v1')
  await apiKey.fill('fixture-only-provider-secret')
  await expect.poll(() => providerForm.evaluate((form: HTMLFormElement) => form.checkValidity())).toBe(true)
  await expect(connect).toBeEnabled()
  await connect.click()
  await expect(page.getByText('Use a valid fixture credential.')).toBeVisible()
  await expect(apiKey).toHaveValue('fixture-only-provider-secret')
  await apiKey.fill('fixture-only-provider-secret-corrected')
  await expect(page.getByText('Use a valid fixture credential.')).toHaveCount(0)
  await expect(connect).toBeEnabled()
  await page.getByRole('button', { name: 'Show secret', exact: true }).click()
  await expect(apiKey).toHaveAttribute('type', 'text')
  await expect(apiKey).toHaveValue('fixture-only-provider-secret-corrected')

  // 已有草稿时返回选择服务先确认；确认放弃后换服务才清空凭据。
  await page.getByRole('tab', { name: 'Choose provider' }).click()
  await page
    .getByRole('alertdialog', { name: 'Discard unsaved changes?' })
    .getByRole('button', { name: 'Discard changes' })
    .click()
  await page.getByRole('button', { name: /^OpenAI Compatible / }).click()
  await expect(apiKey).toHaveAttribute('type', 'password')
  await expect(apiKey).toHaveValue('')
})

test('Provider draft submission locks editing and dismissal until a failed write settles', async ({ page }) => {
  const previewGate = Promise.withResolvers<void>()
  const writeGate = Promise.withResolvers<void>()
  let previewStarted = false
  let writes = 0
  await page.route('**/api/v1/providers/configuration-preview', async (route) => {
    previewStarted = true
    const input = route.request().postDataJSON() as { base_url: string }
    await previewGate.promise
    await route.fulfill({ json: { data: { base_url: input.base_url, issues: [], network_permissions: [] } } })
  })
  await page.route('**/api/v1/providers', async (route) => {
    if (route.request().method() !== 'POST') return route.fallback()
    writes += 1
    await writeGate.promise
    await route.fulfill({ status: 500, json: { error: 'Fixture write rejected' } })
  })

  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await page.getByRole('button', { name: /^OpenAI Compatible / }).click()
  const name = page.getByLabel('Connection name', { exact: true })
  const baseUrl = page.getByLabel('Base URL', { exact: true })
  const secret = page.getByLabel('API key', { exact: true })
  await name.fill('Unsubmitted fixture')
  await baseUrl.fill('https://fixture.example/v1')
  await secret.fill('fixture-only-provider-secret')
  await page.getByRole('button', { name: 'Connect', exact: true }).click()
  try {
    await expect.poll(() => previewStarted).toBe(true)
    await expect(name).toBeDisabled()
    await expect(baseUrl).toBeDisabled()
    await expect(secret).toBeDisabled()
    await expect(page.getByRole('switch')).toBeDisabled()
    await expect(page.getByRole('button', { name: 'Back', exact: true })).toBeDisabled()
    await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeDisabled()
    await expect(page.getByRole('button', { name: 'Close provider setup' })).toBeDisabled()
    await expect(page.getByRole('tab', { name: 'Choose provider' })).toBeDisabled()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('heading', { name: 'Connection details' })).toBeVisible()
    await page.mouse.click(5, 400)
    await expect(page.getByRole('heading', { name: 'Connection details' })).toBeVisible()
    expect(writes).toBe(0)

    previewGate.resolve()
    await expect.poll(() => writes).toBe(1)
    await expect(name).toBeDisabled()
    await expect(page.locator('form button[type="submit"]')).toBeDisabled()
    writeGate.resolve()
    await expect(name).toBeEnabled()
    await expect(name).toHaveValue('Unsubmitted fixture')
    await expect(baseUrl).toHaveValue('https://fixture.example/v1')
    await expect(secret).toHaveValue('fixture-only-provider-secret')
    await expect(page.getByText(/Fixture write rejected/)).toBeVisible()
    expect(writes).toBe(1)
    // 写入失败后草稿仍在：取消先确认，放弃后才关闭。
    await page.getByRole('button', { name: 'Cancel', exact: true }).click()
    await page
      .getByRole('alertdialog', { name: 'Discard unsaved changes?' })
      .getByRole('button', { name: 'Discard changes' })
      .click()
    await expect(page.getByRole('heading', { name: 'Connection details' })).toHaveCount(0)
  } finally {
    previewGate.resolve()
    writeGate.resolve()
  }
})

test('editing a Provider keeps saved credentials write-only', async ({ page }) => {
  const credentialProvider = {
    id: 'credential-provider',
    name: 'Credential Provider',
    vendor: 'openai',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://credential.example/v1',
    use_proxy: false,
    vendor_options: {},
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  let updateBody: Record<string, unknown> | undefined
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [credentialProvider] } })
      return
    }
    if (path === '/providers/credential-provider/models' && request.method() === 'GET') {
      await route.fulfill({
        json: {
          data: {
            models: [
              {
                id: 'test-model',
                name: 'GPT Test',
                available: true,
                source_kind: 'discovered',
                snapshot_state: { type: 'imported', source: { type: 'discovery' } },
                selection_policy: 'auto',
                specification: { limit: { context: 128000 }, modalities: null },
                revision: 1,
              },
            ],
          },
        },
      })
      return
    }
    if (path === '/providers/credential-provider' && request.method() === 'PUT') {
      updateBody = request.postDataJSON()
      await route.fulfill({ json: { data: credentialProvider } })
      return
    }
    await route.fallback()
  })

  await page.goto('/providers/credential-provider?view=connection')
  await expect(page.getByRole('heading', { name: credentialProvider.name })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Upstream models' })).toBeVisible()
  await expect(page.getByLabel('API key', { exact: true })).toHaveValue('')
  await page.getByRole('button', { name: 'Save connection' }).click()

  await expect.poll(() => updateBody).toBeDefined()
  expect(updateBody).not.toHaveProperty('adapter_credentials')
  expect(updateBody).not.toHaveProperty('vendor')
  expect(updateBody).not.toHaveProperty('channel')
  expect(updateBody).toMatchObject({ vendor_options: {} })
})

test('Provider configuration follows plugin field visibility conditions', async ({ page }) => {
  const provider = {
    id: 'conditional-provider',
    name: 'Conditional Provider',
    vendor: 'fixture.conditional',
    channel: 'default',
    protocol: '',
    base_url: 'https://conditional.example',
    use_proxy: false,
    vendor_options: { mode: 'basic' },
    configured_credential_fields: [],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  await page.route('**/api/v1/vendors*', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            provider_id: provider.vendor,
            plugin: { id: provider.vendor, name: 'Conditional fixture plugin' },
            catalog_id: null,
            display_name: 'Conditional fixture',
            description: 'Exercises conditional configuration fields.',
            channels: [
              {
                id: provider.channel,
                name: { 'en-US': 'Default', 'zh-CN': '默认' },
                description: null,
                auth: null,
                protocol: null,
                default_base_url: null,
                capabilities: ['config_validation'],
                model_capabilities: [],
              },
            ],
            capabilities: ['config_validation'],
            config_fields: [
              {
                key: 'mode',
                label: { 'en-US': 'Connection mode', 'zh-CN': '连接模式' },
                kind: {
                  type: 'enum',
                  options: [
                    { value: 'basic', label: { 'en-US': 'Basic', 'zh-CN': '基础' } },
                    { value: 'advanced', label: { 'en-US': 'Advanced', 'zh-CN': '高级' } },
                  ],
                },
                required: true,
                default_json: 'basic',
                secret: false,
              },
              {
                key: 'endpoint_label',
                label: { 'en-US': 'Endpoint label', 'zh-CN': '端点标签' },
                kind: { type: 'string', multiline: false },
                required: true,
                secret: false,
                group: 'advanced',
                visible_when: { field: 'mode', equals: 'advanced' },
              },
            ],
            config_groups: [{ id: 'advanced', label: { 'en-US': 'Advanced settings', 'zh-CN': '高级设置' } }],
            network: { base_url_field: null, extra_origins: [], field_origins: [] },
            data_compat: {
              config_fields_format: 1,
              private_state_format: 1,
              credentials_format: 1,
              model_metadata_format: 1,
            },
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({ json: { data: [provider] } })
  })

  await page.goto(`/providers/${provider.id}?view=connection`)
  await expect(page.getByLabel('Endpoint label', { exact: true })).toHaveCount(0)
  await page.getByLabel('Connection mode', { exact: true }).click()
  await page.getByRole('option', { name: 'Advanced', exact: true }).click()
  await expect(page.getByRole('group', { name: 'Advanced settings' })).toBeVisible()
  await expect(page.getByLabel('Endpoint label', { exact: true })).toBeVisible()
  await page.getByLabel('Connection mode', { exact: true }).click()
  await page.getByRole('option', { name: 'Basic', exact: true }).click()
  await expect(page.getByLabel('Endpoint label', { exact: true })).toHaveCount(0)
})

test('OAuth Provider connection view reconnects the saved vendor channel', async ({ page }) => {
  const provider = {
    id: 'oauth-provider',
    name: 'Codex Account',
    vendor: 'openai-codex',
    protocol: 'open-responses',
    base_url: 'https://api.openai.com/v1',
    use_proxy: false,
    channel: 'codex',
    vendor_options: {},
    configured_credential_fields: [],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    await route.fallback()
  })

  await page.goto(`/providers/${provider.id}?view=connection`)
  await expect(page.getByRole('heading', { name: provider.name })).toBeVisible()
  const initRequest = page.waitForRequest(
    (request) => request.url().endsWith('/api/v1/oauth/sessions/init') && request.method() === 'POST',
  )
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in again' }).click()
  const popup = await popupPromise
  await popup.close()
  expect((await initRequest).postDataJSON()).toMatchObject({
    vendor_id: provider.vendor,
    channel: provider.channel,
    use_proxy: false,
    callback_mode: 'auto',
  })
  await expect(page.getByText('Waiting for authorization…')).toBeVisible()
  await page.getByRole('button', { name: 'Cancel sign-in' }).click()
  await expect(page.getByRole('button', { name: 'Sign in again' })).toBeVisible()
})

test('OAuth service menu reauthorizes without updating the saved connection', async ({ page }) => {
  const provider = {
    id: 'menu-oauth-provider',
    name: 'Expired Codex account',
    vendor: 'openai-codex',
    protocol: 'open-responses',
    base_url: 'https://fixture.invalid/codex',
    use_proxy: true,
    channel: 'codex',
    vendor_options: { workspace: 'keep-existing' },
    configured_credential_fields: [],
    credential_status: 'invalid',
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  const providerWrites: string[] = []
  let bindings = 0
  let cancellations = 0
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (request.method() !== 'GET') providerWrites.push(`${request.method()} ${path}`)
    if (path === '/providers') {
      await route.fulfill({ json: { data: [provider] } })
    } else if (path === `/providers/${provider.id}/oauth/bind`) {
      bindings += 1
      if (bindings === 1) {
        await route.fulfill({ status: 400, json: { error: 'Fixture binding rejected' } })
        return
      }
      provider.credential_status = 'ok'
      await route.fulfill({ json: { data: null } })
    } else {
      await route.fallback()
    }
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/status', async (route) => {
    await route.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } })
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/cancel', async (route) => {
    cancellations += 1
    await route.fulfill({ json: { data: null } })
  })
  await page.goto('/providers')
  await page.getByRole('button', { name: `More actions for ${provider.name}` }).click()
  const initRequest = page.waitForRequest((request) => request.url().endsWith('/oauth/sessions/init'))
  const popup = page.waitForEvent('popup')
  await page.getByRole('menuitem', { name: 'Reauthorize', exact: true }).click()
  await (await popup).close()
  expect((await initRequest).postDataJSON()).toMatchObject({
    provider_id: provider.id,
    vendor_id: provider.vendor,
    channel: provider.channel,
    base_url: provider.base_url,
    use_proxy: true,
    options: provider.vendor_options,
  })
  await expect.poll(() => bindings).toBe(1)
  const authorization = page.getByRole('dialog')
  await expect(authorization.getByRole('alert')).toContainText('Fixture binding rejected')
  expect(provider.credential_status).toBe('invalid')
  await authorization.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect.poll(() => bindings).toBe(2)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(page.getByRole('status').filter({ hasText: 'Credential invalid' })).toHaveCount(0)
  expect(providerWrites).toEqual([
    `POST /providers/${provider.id}/oauth/bind`,
    `POST /providers/${provider.id}/oauth/bind`,
  ])
  expect(cancellations).toBe(0)
})

test('OAuth Provider connection view saves and binds automatically when authorization completes', async ({ page }) => {
  const provider = {
    id: 'oauth-provider',
    name: 'Codex Account',
    vendor: 'openai-codex',
    protocol: 'open-responses',
    base_url: 'https://api.openai.com/v1',
    use_proxy: false,
    channel: 'codex',
    vendor_options: {},
    configured_credential_fields: [],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  let updateBody: Record<string, unknown> | undefined
  let bindBody: Record<string, unknown> | undefined
  await page.route('**/api/v1/providers/oauth-provider/oauth/bind', async (route) => {
    bindBody = route.request().postDataJSON()
    await route.fulfill({ json: { data: null } })
  })
  await page.route('**/api/v1/providers/oauth-provider', async (route) => {
    if (route.request().method() === 'PUT') {
      updateBody = route.request().postDataJSON()
      await route.fulfill({ json: { data: provider } })
      return
    }
    await route.fallback()
  })
  await page.route('**/api/v1/providers', async (route) => {
    if (route.request().method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    await route.fallback()
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/status', async (route) => {
    await route.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } })
  })

  await page.goto(`/providers/${provider.id}?view=connection`)
  await expect(page.getByRole('heading', { name: provider.name })).toBeVisible()
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in again' }).click()
  await (await popupPromise).close()

  await expect.poll(() => updateBody).toMatchObject({ base_url: provider.base_url })
  await expect.poll(() => bindBody).toEqual({ session_id: 'oauth-session-1' })
  await expect(page.locator('[data-sonner-toast]')).toContainText('Connection settings saved')
})

test('Provider draft keeps partial save failure visible and blocks navigation during OAuth binding', async ({
  page,
}) => {
  const provider = {
    id: 'partial-provider',
    name: 'Original account',
    vendor: 'openai-codex',
    protocol: 'open-responses',
    base_url: 'https://api.openai.com/v1',
    use_proxy: false,
    channel: 'codex',
    vendor_options: {},
    configured_credential_fields: [],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  const bindGate = Promise.withResolvers<void>()
  let saved = provider
  let writes = 0
  let binds = 0
  let authorizations = 0
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [saved] } }))
  await page.route('**/api/v1/providers/partial-provider', async (route) => {
    if (route.request().method() !== 'PUT') return route.fallback()
    writes += 1
    saved = { ...provider, ...route.request().postDataJSON(), base_url: 'https://normalized.example/v1' }
    await route.fulfill({ json: { data: saved } })
  })
  await page.route('**/api/v1/providers/partial-provider/oauth/bind', async (route) => {
    binds += 1
    await bindGate.promise
    await route.fulfill({ status: 400, json: { error: 'Fixture authorization rejected' } })
  })
  await page.route('**/api/v1/oauth/sessions/init', async (route) => {
    authorizations += 1
    await route.fallback()
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/status', (route) =>
    route.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } }),
  )

  await page.goto('/providers/partial-provider?view=connection')
  const name = page.getByLabel('Connection name', { exact: true })
  const baseUrl = page.getByLabel('Base URL', { exact: true })
  await name.fill('Updated account')
  await baseUrl.fill('https://draft.example/v1')
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in again' }).click()
  await (await popupPromise).close()
  try {
    await expect.poll(() => binds).toBe(1)
    await expect(name).toBeDisabled()
    await expect(baseUrl).toBeDisabled()
    await expect(page.getByRole('button', { name: 'Use another account' })).toBeDisabled()
    await page.getByRole('link', { name: 'Upstream models', exact: true }).click()
    await expect(page).toHaveURL('/providers/partial-provider?view=connection')
    await expect(page.getByRole('button', { name: 'Save connection' })).toBeDisabled()
    expect(writes).toBe(1)
    expect(authorizations).toBe(1)

    bindGate.resolve()
    await expect(
      page.getByText(/Connection settings were saved, but the authorization could not be attached:/),
    ).toBeVisible()
    await expect(name).toBeEnabled()
    await expect(name).toHaveValue('Updated account')
    await expect(baseUrl).toHaveValue('https://draft.example/v1')
    expect(saved.name).toBe('Updated account')
    expect(writes).toBe(1)
    expect(binds).toBe(1)
    await page.getByRole('link', { name: 'Upstream models', exact: true }).click()
    await expect(page).toHaveURL('/providers/partial-provider?view=models')
  } finally {
    bindGate.resolve()
  }
})

test('Provider Model specifications preserve direction, precision, and unknown states without per-row requests', async ({
  page,
}) => {
  const provider = {
    id: 'specification-provider',
    name: 'Specification Provider',
    vendor: 'openai-compatible',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://specification.example/v1',
    use_proxy: false,
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  const summary = {
    id: 'precision-model',
    name: 'Precision Model',
    available: true,
    source_kind: 'discovered',
    snapshot_state: { type: 'imported', source: { type: 'discovery' } },
    selection_policy: 'auto',
    specification: {
      limit: { context: 1050000 },
      modalities: { input: ['image', 'pdf'], output: ['text'] },
      reasoning_efforts: ['low', 'high'],
    },
    revision: 1,
  }
  const unknownSummary = {
    id: 'unknown-model',
    name: 'Unknown Model',
    available: true,
    source_kind: 'manual',
    snapshot_state: { type: 'edited', source: null },
    selection_policy: 'auto',
    specification: { limit: null, modalities: null },
    revision: 1,
  }
  let detailRequests = 0

  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    if (path === `/providers/${provider.id}/models` && request.method() === 'GET') {
      await route.fulfill({
        json: {
          data: {
            models: [
              summary,
              unknownSummary,
              {
                ...summary,
                id: 'binary-limit',
                name: 'Binary Limit',
                specification: { ...summary.specification, limit: { context: 1048576 } },
              },
            ],
          },
        },
      })
      return
    }
    if (path === `/providers/${provider.id}/model` && request.method() === 'GET') {
      detailRequests += 1
      await route.fulfill({
        json: {
          data: {
            ...summary,
            metadata: {
              id: summary.id,
              name: summary.name,
              limit: summary.specification.limit,
              modalities: summary.specification.modalities,
              reasoning_efforts: summary.specification.reasoning_efforts,
            },
            extensions: {},
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-01T00:00:00Z',
          },
        },
      })
      return
    }
    await route.fallback()
  })

  await page.goto(`/providers/${provider.id}?view=models`)
  const table = page.getByRole('table', { name: 'Models from this provider' })
  const precisionRow = table.getByRole('row').filter({ hasText: /Precision Model.*precision-model/ })
  const unknownRow = table.getByRole('row').filter({ hasText: /Unknown Model.*unknown-model/ })
  const binaryRow = table.getByRole('row').filter({ hasText: /Binary Limit.*binary-limit/ })
  await expect(binaryRow.getByRole('button', { name: 'Context tokens binary-limit', exact: true })).toHaveText(
    '1,048,576',
  )
  const identityCell = precisionRow.getByRole('cell').filter({ hasText: /Precision Model.*precision-model/ })
  await expect(identityCell).not.toContainText('1.05M')
  await expect(identityCell).not.toContainText('Input')
  await expect(precisionRow.getByRole('button', { name: 'Context tokens precision-model', exact: true })).toHaveText(
    '1.05M',
  )
  await expect(precisionRow.getByRole('button', { name: 'Reasoning effort precision-model', exact: true })).toHaveText(
    'low, high',
  )
  const precisionModalities = precisionRow.getByRole('button', { name: 'Modalities precision-model', exact: true })
  const precisionInput = precisionModalities.getByRole('group', { name: 'Input', exact: true })
  const precisionOutput = precisionModalities.getByRole('group', { name: 'Output', exact: true })
  await expect(precisionInput.getByRole('listitem')).toHaveText(['Image', 'PDF'])
  await expect(precisionOutput.getByRole('listitem')).toHaveText(['Text'])
  const unknownModalities = unknownRow.getByRole('button', { name: 'Modalities unknown-model', exact: true })
  await expect(unknownModalities.getByRole('group', { name: 'Input', exact: true })).toContainText('Not registered')
  await expect(unknownModalities.getByRole('group', { name: 'Output', exact: true })).toContainText('Not registered')
  await expect(unknownRow.getByRole('button', { name: 'Context tokens unknown-model', exact: true })).toHaveText(
    'Not registered',
  )
  await expect(unknownRow.getByRole('button', { name: 'Reasoning effort unknown-model', exact: true })).toHaveText(
    'Not registered',
  )
  await expect.poll(() => detailRequests).toBe(0)
})

test('Model specification filters combine all conditions and compose with catalog filters', async ({ page }) => {
  const provider = {
    id: 'filter-provider',
    name: 'Filter Provider',
    vendor: 'openai-compatible',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://filter.example/v1',
    use_proxy: false,
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  const matchingSpecification = {
    limit: { context: 128000 },
    modalities: { input: ['text', 'image', 'pdf'], output: ['text', 'audio'] },
    reasoning_efforts: ['low', 'high'],
  }
  const model = (
    id: string,
    name: string,
    specification:
      | typeof matchingSpecification
      | {
          limit: { context: number | null } | null
          modalities: { input: string[]; output: string[] } | null
          reasoning_efforts?: string[]
        },
    overrides: Partial<{ available: boolean; source_kind: 'discovered' | 'manual' }> = {},
  ) => ({
    id,
    name,
    available: overrides.available ?? true,
    source_kind: overrides.source_kind ?? 'discovered',
    snapshot_state:
      overrides.source_kind === 'manual'
        ? { type: 'edited', source: null }
        : { type: 'imported', source: { type: 'discovery' } },
    selection_policy: 'auto',
    specification,
    revision: 1,
  })
  const models = [
    model('exact-match', 'Exact Match', matchingSpecification, { source_kind: 'manual' }),
    model('below-boundary', 'Below Boundary', {
      ...matchingSpecification,
      limit: { ...matchingSpecification.limit, context: 127999 },
    }),
    model('above-boundary', 'Above Boundary', {
      ...matchingSpecification,
      limit: { ...matchingSpecification.limit, context: 128001 },
    }),
    model('missing-input', 'Missing Input', {
      ...matchingSpecification,
      modalities: { input: ['text', 'image'], output: ['text', 'audio', 'pdf'] },
    }),
    model('missing-output', 'Missing Output', {
      ...matchingSpecification,
      modalities: { input: ['text', 'image', 'pdf', 'audio'], output: ['text'] },
    }),
    model('missing-effort', 'Missing Effort', { ...matchingSpecification, reasoning_efforts: ['low'] }),
    model('unknown-values', 'Unknown Values', { limit: null, modalities: null }),
    model('source-decoy', 'Exact Match Synced', matchingSpecification),
    model('usage-decoy', 'Exact Match Unused', matchingSpecification, { source_kind: 'manual' }),
    model('status-decoy', 'Exact Match Retired', matchingSpecification, { available: false, source_kind: 'manual' }),
  ]
  const referencedRoute = {
    id: 'filter-route',
    model_id: 'filter-route',
    display_name: null,
    balance: 'priority',
    target_provider: provider.id,
    target_model: 'exact-match',
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    targets: [
      {
        id: 'filter-target',
        model_id: 'filter-route',
        provider_id: provider.id,
        model: 'exact-match',
        enabled: true,
        priority: 1,
        created_at: '2026-01-01T00:00:00Z',
      },
    ],
  }
  let detailRequests = 0

  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    if (path === `/providers/${provider.id}/models` && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models } } })
      return
    }
    if (path === `/providers/${provider.id}/model` && request.method() === 'GET') detailRequests += 1
    await route.fallback()
  })
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [referencedRoute] } }))

  await page.goto(`/providers/${provider.id}?view=models`)
  const table = page.getByRole('table', { name: 'Models from this provider' })
  const filterButton = table.getByRole('button', { name: 'Show filter menu for Model specification' })
  await filterButton.click()
  const specificationDialog = page.getByRole('dialog', { name: 'Filter by Model specification' })
  await expect(specificationDialog).toContainText('Match all selected conditions')

  await specificationDialog.getByRole('spinbutton', { name: 'Minimum context tokens' }).fill('128000')
  await expect(table.getByRole('row').filter({ hasText: 'Unknown Values' })).toBeVisible()
  await page.keyboard.press('Escape')
  await filterButton.click()
  await expect(specificationDialog.getByRole('spinbutton', { name: 'Minimum context tokens' })).toHaveValue('')

  await specificationDialog.getByRole('combobox', { name: 'Context preset' }).click()
  await page.getByRole('option', { name: '128K', exact: true }).click()
  const inputGroup = specificationDialog.getByRole('group', { name: 'Input modalities' })
  await inputGroup.getByRole('checkbox', { name: 'Image', exact: true }).check()
  await inputGroup.getByRole('checkbox', { name: 'PDF', exact: true }).check()
  const outputGroup = specificationDialog.getByRole('group', { name: 'Output modalities' })
  await outputGroup.getByRole('checkbox', { name: 'Text', exact: true }).check()
  await outputGroup.getByRole('checkbox', { name: 'Audio', exact: true }).check()
  await specificationDialog.getByRole('checkbox', { name: 'high', exact: true }).check()
  await specificationDialog.getByRole('button', { name: 'Apply', exact: true }).click()
  await expect(specificationDialog).toBeHidden()

  await expect(table.getByRole('row').filter({ hasText: 'Exact Match' })).toHaveCount(3)
  await expect(table.getByRole('row').filter({ hasText: 'Below Boundary' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Above Boundary' })).toBeVisible()
  await expect(table.getByRole('row').filter({ hasText: 'Missing Input' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Missing Output' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Missing Effort' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Unknown Values' })).toHaveCount(0)

  await filterButton.click()
  await specificationDialog.getByRole('spinbutton', { name: 'Minimum context tokens' }).fill('1000000')
  await inputGroup.getByRole('checkbox', { name: 'PDF', exact: true }).uncheck()
  await page.keyboard.press('Escape')
  await expect(table.getByRole('row').filter({ hasText: /Exact Match.*exact-match/ })).toBeVisible()

  await filterButton.click()
  await expect(specificationDialog.getByRole('spinbutton', { name: 'Minimum context tokens' })).toHaveValue('128000')
  await expect(inputGroup.getByRole('checkbox', { name: 'PDF', exact: true })).toBeChecked()
  await specificationDialog.getByRole('combobox', { name: 'Context preset' }).click()
  await page.getByRole('option', { name: 'Custom', exact: true }).click()
  await specificationDialog.getByRole('spinbutton', { name: 'Minimum context tokens' }).fill('128000')
  await specificationDialog.getByRole('button', { name: 'Apply', exact: true }).click()
  await expect(table.getByRole('row').filter({ hasText: 'Exact Match' })).toHaveCount(3)
  await expect(table.getByRole('row').filter({ hasText: 'Below Boundary' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Above Boundary' })).toBeVisible()
  await expect(table.getByRole('row').filter({ hasText: 'Unknown Values' })).toHaveCount(0)

  await page.locator('#provider-model-search-desktop').fill('Exact Match')
  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  const availabilityDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityDialog.locator('[data-slot="select-trigger"][aria-label="Model availability"]').click()
  await page.getByRole('option', { name: 'Available', exact: true }).click()
  await availabilityDialog.getByRole('button', { name: 'Apply' }).click()
  await page.getByRole('button', { name: 'Show filter menu for How models were added' }).click()
  const sourceDialog = page.getByRole('dialog', { name: 'Filter by How models were added' })
  await sourceDialog.locator('[data-slot="select-trigger"][aria-label="How models were added"]').click()
  await page.getByRole('option', { name: 'Added manually', exact: true }).click()
  await sourceDialog.getByRole('button', { name: 'Apply' }).click()
  await page.getByRole('button', { name: 'Show filter menu for Model usage' }).click()
  const usageDialog = page.getByRole('dialog', { name: 'Filter by Model usage' })
  await usageDialog.locator('[data-slot="select-trigger"][aria-label="Model usage"]').click()
  await page.getByRole('option', { name: 'In use', exact: true }).click()
  await usageDialog.getByRole('button', { name: 'Apply' }).click()
  await expect(table.getByRole('row').filter({ hasText: /Exact Match.*exact-match/ })).toHaveCount(1)
  await expect(table.getByRole('row').filter({ hasText: 'Exact Match Synced' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Exact Match Unused' })).toHaveCount(0)
  await expect(table.getByRole('row').filter({ hasText: 'Exact Match Retired' })).toHaveCount(0)

  await filterButton.click()
  await specificationDialog.getByRole('spinbutton', { name: 'Minimum context tokens' }).fill('128001')
  await specificationDialog.getByRole('button', { name: 'Apply', exact: true }).click()
  await expect(table).toContainText('No models match these filters.')
  await filterButton.click()
  await specificationDialog.getByRole('button', { name: 'Clear', exact: true }).click()
  await expect(specificationDialog).toBeHidden()
  await expect(table.getByRole('row').filter({ hasText: /Exact Match.*exact-match/ })).toHaveCount(1)
  await expect(table.getByRole('row').filter({ hasText: 'Unknown Values' })).toHaveCount(0)
  await page.locator('#provider-model-search-desktop').fill('__no_match__')
  await expect(table).toContainText('No models match these filters.')
  await table.getByRole('button', { name: 'Clear filters', exact: true }).click()
  await expect(table.getByRole('row').filter({ hasText: 'Unknown Values' })).toBeVisible()

  await page.setViewportSize({ width: 390, height: 844 })
  const mobileList = page.locator('.route-mobile-list')
  await mobileList.getByRole('button', { name: 'Filter models', exact: true }).click()
  const mobileFilters = page.getByRole('dialog', { name: 'Filter models', exact: true })
  await mobileFilters.getByRole('spinbutton', { name: 'Minimum context tokens' }).fill('128000')
  await mobileFilters.getByRole('button', { name: 'Show models', exact: true }).click()
  await expect(mobileList).not.toContainText('Unknown Values')
  await expect(mobileList).not.toContainText('Below Boundary')
  await expect(mobileList).toContainText('Exact Match')
  await mobileList.getByRole('button', { name: 'Clear filters', exact: true }).click()
  await expect(mobileList).toContainText('Unknown Values')
  await expect.poll(() => detailRequests).toBe(0)
})

test('Provider Model editor uses structured fields and preserves exact decimal input', async ({ page }) => {
  const provider = {
    id: 'visual-provider',
    name: 'Visual Provider',
    vendor: 'openai',
    protocol: 'openai-compatible',
    base_url: 'https://models.example/v1',
    use_proxy: false,
    channel: 'default',
    vendor_options: {},
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  const summary = {
    id: 'gpt-test',
    name: 'GPT Test',
    available: true,
    source_kind: 'discovered',
    snapshot_state: { type: 'imported', source: { type: 'discovery' } },
    selection_policy: 'auto',
    specification: { limit: { context: 128000 }, modalities: null },
    revision: 1,
  }
  const unavailableSummary = { ...summary, id: 'gpt-retired', name: 'GPT Retired', available: false }
  const detail = {
    ...summary,
    metadata: {
      id: 'gpt-test',
      name: 'GPT Test',
      description: 'Structured metadata',
      family: 'gpt',
      knowledge: '2025-01',
      release_date: '2026-01-01',
      last_updated: '2026-02-01',
      open_weights: true,
      modalities: { input: ['text', 'image', 'binary'], output: ['text'] },
      limit: { context: 128000 },
      cost: {
        input: 0.25,
        output: 1,
        cache_read: 0.025,
        cache_write: 0.5,
        context_over_200k: { input: 5, output: 22.5 },
        tiers: [{ tier: { type: 'context', size: 272000 }, input: 6, output: 24, cache_read: 0.6, cache_write: 12 }],
      },
      reasoning_efforts: ['low', 'medium', 'high', 'future'],
      vendor_extension: { mode: 'private' },
    },
    extensions: { vendor_extension: { mode: 'private' } },
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  let updateBody = ''
  const prepareBodies: Array<{ model_id: string; template_id?: string }> = []

  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    if (path === '/providers/visual-provider/models' && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models: [unavailableSummary, summary] } } })
      return
    }
    if (
      path === '/providers/visual-provider/model' &&
      url.searchParams.get('model') === 'gpt-test' &&
      request.method() === 'GET'
    ) {
      await route.fulfill({ json: { data: detail } })
      return
    }
    if (path === '/providers/visual-provider/model' && request.method() === 'PUT') {
      updateBody = request.postData() ?? ''
      await route.fulfill({ json: { data: { ...detail, revision: 2 } } })
      return
    }
    if (path === '/providers/visual-provider/model/prepare' && request.method() === 'POST') {
      const body = request.postDataJSON() as { model_id: string; template_id?: string }
      prepareBodies.push(body)
      if (body.template_id === 'anthropic/claude-opus-4.6') {
        await route.fulfill({
          status: 404,
          json: { code: 'CATALOG_MODEL_NOT_FOUND', error: 'Selected model is no longer available.' },
        })
        return
      }
      await route.fulfill({
        json: {
          data: {
            id: body.model_id,
            snapshot_state: body.template_id
              ? { type: 'imported', source: { type: 'canonical', model_id: body.template_id } }
              : { type: 'unregistered' },
            available: true,
            source_kind: 'manual',
            selection_policy: 'auto',
            metadata: {
              id: body.model_id,
              name: body.template_id ? 'GPT-5.4' : 'Known New Model',
              reasoning_efforts: ['high'],
            },
            extensions: body.template_id ? { benchmarks: [{ name: 'Template benchmark' }] } : {},
            revision: 1,
            created_at: '',
            updated_at: '',
          },
        },
      })
      return
    }
    await route.fallback()
  })

  await page.goto('/providers/visual-provider?view=models')
  const availableModelRow = page.getByRole('row').filter({ hasText: /GPT Test.*gpt-test/ })
  await expect(availableModelRow).toBeVisible()
  await expect(page.getByRole('row').filter({ hasText: /GPT Retired.*gpt-retired/ })).toHaveCount(0)
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveCount(0)
  await expect(page.getByText('Shown when adding models', { exact: true })).toHaveCount(0)
  await availableModelRow
    .getByRole('cell')
    .filter({ hasText: /GPT Test.*gpt-test/ })
    .click()

  // 编辑器替换模型清单后，模型 ID 只出现在编辑器页头：桌面表格与移动列表也在 DOM 中，需限定可见表面。
  const modelEditor = page.locator('section[aria-labelledby="provider-model-editor-title"]')
  await expect(modelEditor.getByRole('heading', { name: 'GPT Test', exact: true })).toBeVisible()
  await expect(modelEditor.getByRole('button', { name: 'gpt-test', exact: true })).toBeVisible()
  const extensionFields = page.getByRole('button', { name: 'Extension fields (read only) · 1', exact: true })
  await expect(extensionFields).toHaveAttribute('aria-expanded', 'false')
  await extensionFields.click()
  await expect(extensionFields).toHaveAttribute('aria-expanded', 'true')
  const extensionPreview = extensionFields.locator('..').locator('pre')
  await expect(extensionPreview).toContainText('"vendor_extension"')
  await expect(extensionPreview).toContainText('"private"')
  await expect(page.locator('#provider-model-tier-0')).toHaveValue('272000')
  const inputModalities = page.locator('[data-modality-select="input"]')
  await expect(inputModalities.getByRole('listitem')).toHaveText(['Text', 'Image', 'binary'])
  await inputModalities.click()
  for (const value of ['Text', 'Image', 'Audio', 'Video', 'PDF', 'binary']) {
    await expect(page.getByRole('option', { name: value, exact: true })).toBeVisible()
  }
  await page.getByRole('option', { name: 'Audio', exact: true }).click()
  await page.keyboard.press('Escape')

  const outputModalities = page.locator('[data-modality-select="output"]')
  await expect(outputModalities.getByRole('listitem')).toHaveText(['Text'])
  await outputModalities.click()
  for (const value of ['Text', 'Image', 'Audio', 'Video', 'PDF']) {
    await expect(page.getByRole('option', { name: value, exact: true })).toBeVisible()
  }
  await page.getByRole('option', { name: 'Image', exact: true }).click()
  await page.keyboard.press('Escape')
  const effortValuesSelect = page.locator('[data-effort-values-select]')
  await expect(effortValuesSelect).toContainText('low, medium, high, future')
  await effortValuesSelect.click()
  for (const value of ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'future']) {
    await expect(page.getByRole('option', { name: value, exact: true })).toBeVisible()
  }
  await page.getByRole('option', { name: 'none', exact: true }).click()
  await page.keyboard.press('Escape')
  await page.locator('#provider-model-cost-input').fill('0.123456789012345678')
  await expect(page.locator('#provider-model-cost-input')).toHaveValue('0.123456789012345678')
  await page.getByRole('button', { name: 'Save model' }).click()

  await expect.poll(() => updateBody).toContain('0.123456789012345678')
  expect(updateBody).toContain('"vendor_extension":{"mode":"private"}')
  expect(updateBody).toContain('"family":"gpt"')
  expect(updateBody).toContain('"knowledge":"2025-01"')
  expect(updateBody).toContain('"release_date":"2026-01-01"')
  expect(updateBody).toContain('"last_updated":"2026-02-01"')
  const savedMetadata = JSON.parse(updateBody).metadata
  expect(savedMetadata.open_weights).toBe(true)
  expect(savedMetadata.modalities).toEqual({ input: ['text', 'image', 'audio', 'binary'], output: ['text', 'image'] })
  expect(savedMetadata.cost).toEqual({
    input: Number('0.123456789012345678'),
    output: 1,
    cache_read: 0.025,
    cache_write: 0.5,
    context_over_200k: detail.metadata.cost.context_over_200k,
    tiers: detail.metadata.cost.tiers,
  })
  expect(savedMetadata.reasoning_efforts).toEqual(['none', 'low', 'medium', 'high', 'future'])

  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await page.getByRole('button', { name: 'Add manually', exact: true }).click()
  const manualModelPicker = page.locator('#manual-provider-model-search')
  await manualModelPicker.click()
  const manualModelSearch = page.getByPlaceholder('Search model template')
  await expect(manualModelSearch).toHaveAttribute('aria-label', 'Search model template')
  await manualModelSearch.fill('openai/gpt-5.4')
  await page.getByRole('option', { name: /GPT-5\.4.*openai\/gpt-5\.4/ }).click()
  await manualModelPicker.click()
  await page.getByRole('button', { name: 'Clear selected model' }).click()
  await expect(page.getByRole('button', { name: 'Continue' })).toBeDisabled()
  await manualModelPicker.click()
  await manualModelSearch.fill('openai/gpt-5.4')
  await page.getByRole('option', { name: /GPT-5\.4.*openai\/gpt-5\.4/ }).click()
  await page.getByRole('button', { name: 'Continue' }).click()
  await expect.poll(() => prepareBodies[0]).toEqual({ model_id: 'gpt-5.4', template_id: 'openai/gpt-5.4' })
  await expect(page.getByRole('dialog').getByText('gpt-5.4', { exact: true })).toBeVisible()
  await expect(page.locator('#provider-model-name')).toHaveValue('GPT-5.4')
  await expect(extensionFields).toHaveAttribute('aria-expanded', 'false')
  await extensionFields.click()
  await expect(extensionFields).toHaveAttribute('aria-expanded', 'true')
  await expect(extensionPreview).toContainText('"benchmarks"')
  await expect(extensionPreview).toContainText('Template benchmark')

  await page.getByRole('button', { name: 'Close model editor' }).click()
  await page.getByRole('button', { name: 'Add manually', exact: true }).click()
  await manualModelPicker.click()
  await manualModelSearch.fill('openai/gpt-5.3-codex-spark')
  await page.getByRole('option', { name: /GPT-5\.3 Codex Spark.*openai\/gpt-5\.3-codex-spark/ }).click()
  const [manualDialogBounds, manualPickerBounds] = await Promise.all([
    page.getByRole('dialog', { name: 'Add a model manually' }).boundingBox(),
    manualModelPicker.boundingBox(),
  ])
  if (!manualDialogBounds || !manualPickerBounds) {
    throw new Error('Add-model picker is not visible')
  }
  expect(manualPickerBounds.x + manualPickerBounds.width).toBeLessThanOrEqual(
    manualDialogBounds.x + manualDialogBounds.width,
  )
  await manualModelPicker.click()
  await page.getByRole('button', { name: 'Clear selected model' }).click()
  await manualModelPicker.click()
  await manualModelSearch.fill('anthropic/claude-opus-4.6')
  await page.getByRole('option', { name: /Claude Opus 4\.6.*anthropic\/claude-opus-4\.6/ }).click()
  await page.getByRole('button', { name: 'Continue' }).click()
  await expect(page.getByText('The selected model is no longer available. Search for another model.')).toBeVisible()
  await expect(page.getByRole('dialog', { name: 'Add a model manually' })).toBeVisible()
  await expect(manualModelPicker).toHaveText(/Claude Opus 4\.6.*anthropic\/claude-opus-4\.6/)
})

test('OAuth Provider configuration allows manual completion while the localhost listener is active', async ({
  page,
}) => {
  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await page.getByRole('button', { name: /Codex.*OAuth account/ }).click()

  await expect(page.getByLabel('Connection name')).toHaveValue('OpenAI Codex')

  const initRequest = page.waitForRequest(
    (request) => request.url().endsWith('/api/v1/oauth/sessions/init') && request.method() === 'POST',
  )
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in with OAuth' }).click()
  const popup = await popupPromise
  await popup.close()

  expect((await initRequest).postDataJSON()).toMatchObject({
    vendor_id: 'openai-codex',
    channel: 'codex',
    use_proxy: false,
    callback_mode: 'auto',
  })
  await expect(page.getByText('Waiting for authorization…')).toBeVisible()
  await expect(page.getByLabel('Callback URL')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Complete', exact: true })).toBeDisabled()
  await expect(page.getByRole('button', { name: 'Reopen sign-in page' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Cancel sign-in' })).toBeVisible()

  let createBody: Record<string, unknown> | undefined
  const createGate = Promise.withResolvers<void>()
  await page.route('**/api/v1/providers/oauth', async (route) => {
    createBody = route.request().postDataJSON()
    await createGate.promise
    await route.fulfill({ json: { data: {} } })
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/complete', async (route) => {
    await page.route('**/api/v1/oauth/sessions/oauth-session-1/status', async (statusRoute) => {
      await statusRoute.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } })
    })
    await route.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } })
  })
  const callbackUrl = 'http://localhost:1457/auth/callback?code=test-code&state=test-state'
  const completeRequest = page.waitForRequest(
    (request) =>
      request.url().endsWith('/api/v1/oauth/sessions/oauth-session-1/complete') && request.method() === 'POST',
  )
  await page.getByLabel('Callback URL').fill(callbackUrl)
  await page.getByRole('button', { name: 'Complete', exact: true }).click()
  expect((await completeRequest).postDataJSON()).toEqual({ input: { type: 'callback_url', value: callbackUrl } })
  await expect(page.getByText('Authorization complete', { exact: true })).toBeVisible()
  await expect(page.getByLabel('Callback URL')).toHaveCount(0)
  await expect.poll(() => createBody).toMatchObject({ session_id: 'oauth-session-1' })
  createGate.resolve()
})

test('completed OAuth satisfies session credential fields and submits the provider form', async ({ page }) => {
  const manualInput = {
    type: 'text',
    label: { 'en-US': 'Devin token', 'zh-CN': 'Devin 令牌' },
    description: {
      'en-US': 'Paste the local fixture token returned by authorization.',
      'zh-CN': '粘贴授权返回的本地测试令牌。',
    },
    secret: true,
  } as const
  const descriptor = {
    provider_id: 'devin',
    plugin: { id: 'devin', name: 'Devin' },
    catalog_id: 'devin',
    display_name: 'Devin',
    description: 'Local OAuth form regression fixture.',
    channels: [
      {
        id: 'devin',
        name: { 'en-US': 'Devin', 'zh-CN': 'Devin' },
        description: { 'en-US': 'OAuth account', 'zh-CN': 'OAuth 账号' },
        auth: {
          flow: 'authorization_code',
          callback: {
            bind_host: '127.0.0.1',
            redirect_host: '127.0.0.1',
            path: '/callback',
            port: { kind: 'dynamic' },
            manual_redirect_uri: 'chisel-show-auth-token',
          },
          manual_input: manualInput,
        },
        protocol: 'devin-connect',
        default_base_url: 'https://server.codeium.test',
        capabilities: ['infer', 'auth_oauth', 'config_validation'],
        model_capabilities: [],
      },
    ],
    capabilities: ['infer', 'auth_oauth', 'config_validation'],
    config_fields: [
      {
        key: 'apiKey',
        label: { 'en-US': 'Session token', 'zh-CN': '会话令牌' },
        description: {
          'en-US': 'OAuth-created session token or a manually pasted token.',
          'zh-CN': 'OAuth 创建的会话令牌或手动粘贴的令牌。',
        },
        kind: { type: 'string', multiline: false },
        required: true,
        secret: true,
        max_length: 16_384,
      },
    ],
    config_groups: [],
    network: {
      base_url_field: null,
      extra_origins: [{ scheme: 'https', host: 'server.codeium.test' }],
      field_origins: [],
    },
    data_compat: { config_fields_format: 1, private_state_format: 1, credentials_format: 1, model_metadata_format: 1 },
  } satisfies ProviderMetadata
  const fixtureSecret = 'fixture-only-devin-session-token'
  let oauthReady = false
  let initBody: Record<string, unknown> | undefined
  let createBody: Record<string, unknown> | undefined

  await page.route('**/api/v1/vendors*', (route) => route.fulfill({ json: { data: [descriptor] } }))
  await page.route('**/api/v1/providers/configuration-preview', async (route) => {
    const input = route.request().postDataJSON() as { base_url: string; credentials: Record<string, unknown> }
    expect(input.credentials).toEqual({})
    await route.fulfill({ json: { data: { base_url: input.base_url, issues: [], network_permissions: [] } } })
  })
  await page.route('**/api/v1/oauth/sessions/init', async (route) => {
    initBody = route.request().postDataJSON()
    await route.fulfill({
      json: {
        data: {
          session_id: 'devin-oauth-session',
          vendor_id: 'devin',
          channel: 'devin',
          flow: 'authorization_code',
          auth_url: null,
          callback_mode: 'auto',
          listener_state: 'listening',
          listener_port: 1457,
          redirect_uri: 'http://127.0.0.1:1457/callback',
          fallback_reason: null,
          manual_input: manualInput,
          expires_in: 600,
          interval: 2,
        },
      },
    })
  })
  await page.route('**/api/v1/oauth/sessions/devin-oauth-session/status', async (route) => {
    await route.fulfill({
      json: {
        data: oauthReady
          ? { status: 'ready', expires_in: 600 }
          : {
              status: 'pending',
              auth_url: null,
              callback_mode: 'auto',
              listener_state: 'listening',
              listener_port: 1457,
              redirect_uri: 'http://127.0.0.1:1457/callback',
              fallback_reason: null,
              manual_input: manualInput,
              expires_in: 600,
              interval: 2,
            },
      },
    })
  })
  await page.route('**/api/v1/oauth/sessions/devin-oauth-session/complete', async (route) => {
    expect(route.request().postDataJSON()).toEqual({ input: { type: 'manual', value: fixtureSecret } })
    oauthReady = true
    await route.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } })
  })
  const createGate = Promise.withResolvers<void>()
  await page.route('**/api/v1/providers/oauth', async (route) => {
    createBody = route.request().postDataJSON()
    await createGate.promise
    await route.fulfill({
      json: {
        data: {
          id: 'devin-provider',
          name: 'Devin',
          vendor: 'devin',
          channel: 'devin',
          protocol: 'devin-connect',
          base_url: 'https://server.codeium.test',
          use_proxy: false,
          vendor_options: {},
          configured_credential_fields: ['apiKey'],
          is_enabled: true,
          created_at: '2026-09-21T00:00:00Z',
          updated_at: '2026-09-21T00:00:00Z',
        },
      },
    })
  })

  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await page.getByRole('button', { name: /Devin.*OAuth account/ }).click()

  const providerForm = page.locator('form.route-overlay-form')
  const sessionToken = page.getByLabel('Session token', { exact: true })
  await expect(sessionToken).toHaveAttribute('required', '')
  await expect.poll(() => providerForm.evaluate((form: HTMLFormElement) => form.checkValidity())).toBe(false)

  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in with OAuth' }).click()
  await popupPromise
  await page.getByLabel('Devin token', { exact: true }).fill(fixtureSecret)
  await page.getByRole('button', { name: 'Complete', exact: true }).click()
  await expect(page.getByText('Authorization complete', { exact: true })).toBeVisible()

  expect(initBody).toMatchObject({ vendor_id: 'devin', channel: 'devin', credentials: {} })
  expect(JSON.stringify(initBody)).not.toContain(fixtureSecret)
  await expect(sessionToken).toHaveValue('')
  await expect(sessionToken).not.toHaveAttribute('required', '')
  await expect.poll(() => providerForm.evaluate((form: HTMLFormElement) => form.checkValidity())).toBe(true)

  // Authorization completion saves and binds the provider without a separate submit.
  await expect
    .poll(() => createBody)
    .toMatchObject({
      session_id: 'devin-oauth-session',
      input: {
        source: { type: 'custom', vendor: 'devin', channel: 'devin' },
        credential: { type: 'none' },
        vendor_options: {},
      },
    })
  expect(JSON.stringify(createBody)).not.toContain(fixtureSecret)
  createGate.resolve()
  await expect(page).toHaveURL(/\/providers\/devin-provider\?view=connection/)
})

test('Provider draft discards OAuth initialization completed after configuration changes', async ({ page }) => {
  const initGate = Promise.withResolvers<void>()
  let initializing = false
  let cancelled = false
  let writes = 0
  await page.route('**/api/v1/oauth/sessions/init', async (route) => {
    initializing = true
    await initGate.promise
    await route.fallback()
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/cancel', async (route) => {
    cancelled = true
    await route.fulfill({ json: { data: null } })
  })
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/status', (route) =>
    route.fulfill({ json: { data: { status: 'ready', expires_in: 600 } } }),
  )
  await page.route('**/api/v1/providers/oauth', async (route) => {
    writes += 1
    await route.fulfill({ status: 400, json: { error: 'Stale authorization must not save' } })
  })

  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await page.getByRole('button', { name: /OpenAI.*Codex.*OAuth account/ }).click()
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in with OAuth' }).click()
  const popup = await popupPromise
  try {
    await expect.poll(() => initializing).toBe(true)
    await page.getByLabel('Base URL', { exact: true }).fill('https://new-draft.example/v1')
    initGate.resolve()
    await expect.poll(() => cancelled).toBe(true)
    await expect(page.getByRole('button', { name: 'Sign in with OAuth' })).toBeEnabled()
    await expect(page.getByLabel('Base URL', { exact: true })).toHaveValue('https://new-draft.example/v1')
    expect(writes).toBe(0)
  } finally {
    initGate.resolve()
    if (!popup.isClosed()) await popup.close()
  }
})

test('manual OAuth fallback shows one full callback URL field', async ({ page }) => {
  await page.route('**/api/v1/oauth/sessions/oauth-session-1/complete', async (route) => {
    await route.fulfill({
      status: 400,
      json: {
        error: JSON.stringify({
          code: 'AUTH_CALLBACK_STATE_MISMATCH',
          message: 'callback state does not match',
          params: {},
        }),
      },
    })
  })
  await page.route('**/api/v1/oauth/sessions/init', async (route) => {
    await route.fulfill({
      json: {
        data: {
          session_id: 'oauth-session-1',
          vendor_id: 'openai-codex',
          channel: 'codex',
          flow: 'authorization_code',
          auth_url: 'https://auth.openai.example/authorize',
          callback_mode: 'manual',
          listener_state: 'not_started',
          listener_port: null,
          redirect_uri: 'http://localhost:1457/auth/callback',
          fallback_reason: 'callback_ports_unavailable',
          expires_in: 600,
          interval: 2,
        },
      },
    })
  })
  await page.addInitScript(() => {
    window.open = (url) => {
      sessionStorage.setItem('opened-oauth-url', String(url))
      return null
    }
  })
  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await page.getByRole('button', { name: /Codex.*OAuth account/ }).click()
  await page.getByRole('button', { name: 'Sign in with OAuth' }).click()

  await expect
    .poll(() => page.evaluate(() => sessionStorage.getItem('opened-oauth-url')))
    .toBe('https://auth.openai.example/authorize')
  await expect(page.getByLabel('Callback URL')).toHaveCount(1)
  await expect(page.getByRole('button', { name: 'Complete' })).toBeDisabled()
  const callbackUrl = 'http://localhost:1457/auth/callback?code=bad&state=wrong'
  await page.getByLabel('Callback URL').fill(callbackUrl)
  await page.getByRole('button', { name: 'Complete' }).click()
  await expect(page.getByText('The OAuth callback state is invalid. Start authorization again.')).toBeVisible()
  await expect(page.getByLabel('Callback URL')).toHaveValue(callbackUrl)
})

test('Provider detail separates connection, inventory, references, and guarded model drafts', async ({ page }) => {
  await page.setViewportSize({ width: 1920, height: 1080 })
  const provider = {
    id: 'provider-detail',
    name: 'Provider Detail',
    vendor: 'openai',
    protocol: 'openai-compatible',
    base_url: 'https://provider.example/v1',
    use_proxy: false,
    channel: 'default',
    vendor_options: {},
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  const available = {
    id: 'openai/gpt-test',
    name: 'GPT Test',
    available: true,
    source_kind: 'discovered',
    snapshot_state: { type: 'imported', source: { type: 'discovery' } },
    selection_policy: 'auto',
    specification: { limit: { context: 128000 }, modalities: null },
    revision: 1,
  }
  const unavailable = {
    ...available,
    id: 'retired-model',
    name: 'Retired Model',
    available: false,
    source_kind: 'manual',
    snapshot_state: { type: 'edited', source: null },
  }
  const detail = {
    ...available,
    can_reimport: true,
    metadata: {
      id: available.id,
      name: available.name,
      description: 'Editable metadata',
      modalities: { input: ['text'], output: ['text'] },
      limit: { context: 128000 },
    },
    extensions: {},
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  const routeModel = {
    id: 'route-detail',
    model_id: 'client-model',
    display_name: 'Client model',
    balance: 'priority',
    target_provider: provider.id,
    target_model: unavailable.id,
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    targets: [
      {
        id: 'target-detail',
        model_id: 'route-detail',
        provider_id: provider.id,
        model: unavailable.id,
        weight: 100,
        priority: 1,
        created_at: '2026-08-17T00:00:00Z',
      },
    ],
  }
  let selectionBody: Record<string, unknown> | undefined
  let routeQueryFails = false

  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    if (path === `/providers/${provider.id}/models` && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models: [available, unavailable] } } })
      return
    }
    if (path === `/providers/${provider.id}/model` && request.method() === 'GET') {
      const modelId = url.searchParams.get('model')
      await route.fulfill({
        json: {
          data: {
            ...detail,
            id: modelId,
            source_kind: modelId === unavailable.id ? unavailable.source_kind : detail.source_kind,
            snapshot_state: modelId === unavailable.id ? unavailable.snapshot_state : detail.snapshot_state,
            available: modelId === unavailable.id ? unavailable.available : detail.available,
            can_reimport: modelId === unavailable.id ? false : detail.can_reimport,
            metadata: {
              ...detail.metadata,
              id: modelId,
              name: modelId === unavailable.id ? unavailable.name : detail.metadata.name,
            },
          },
        },
      })
      return
    }
    if (path === `/providers/${provider.id}/models/sync` && request.method() === 'POST') {
      await route.fulfill({
        status: 502,
        json: {
          code: 'CATALOG_SCOPE_REFRESH_FAILED',
          error: 'Provider Catalog scope refresh failed. Provider Models were not changed; retry the operation.',
        },
      })
      return
    }
    if (path === `/providers/${provider.id}/model/reimport` && request.method() === 'POST') {
      await route.fulfill({
        status: 404,
        json: {
          code: 'CATALOG_ENTRY_NOT_FOUND',
          error: 'Provider Catalog Entry was not found in the active catalog revision.',
        },
      })
      return
    }
    if (path === `/providers/${provider.id}/model/selection` && request.method() === 'PUT') {
      selectionBody = request.postDataJSON()
      await route.fulfill({ json: { data: { ...detail, selection_policy: selectionBody?.policy, revision: 2 } } })
      return
    }
    await route.fallback()
  })
  await page.route('**/api/v1/models', async (route) => {
    if (routeQueryFails) {
      await route.fulfill({ status: 500, json: { error: 'Route dependency query failed' } })
    } else {
      await route.fulfill({ json: { data: [routeModel] } })
    }
  })

  await page.goto(`/providers/${provider.id}?view=models`)
  await expect(page.getByRole('heading', { name: provider.name })).toBeVisible()
  const providerBreadcrumb = page.getByRole('navigation', { name: 'Breadcrumb' })
  await expect(providerBreadcrumb.getByRole('link', { name: 'Providers' })).toHaveAttribute('href', '/providers')
  await expect(providerBreadcrumb.getByText(provider.name, { exact: true })).toHaveAttribute('aria-current', 'page')
  const providerViews = page.getByRole('navigation', { name: 'Provider details' })
  await expect(providerViews).toHaveCSS('border-bottom-width', '0px')
  await providerViews.getByRole('link', { name: 'Connection settings' }).click()
  const connectionSection = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Connection settings', level: 2 }) })
  // 表单位于 60rem 设置列，不随 120rem 容器拉伸。
  await expect
    .poll(async () => Math.round((await connectionSection.locator('form').boundingBox())?.width ?? 0))
    .toBeLessThanOrEqual(960)
  await providerViews.getByRole('link', { name: 'Upstream models' }).click()
  await expect(page.getByRole('link', { name: 'Connection settings' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Upstream models' })).toHaveAttribute('aria-current', 'page')
  const modelTable = page.getByRole('table', { name: 'Models from this provider' })
  const modelTableContainer = modelTable.locator('xpath=..')
  expect(await modelTableContainer.evaluate((element) => element.scrollWidth)).toBeLessThanOrEqual(
    await modelTableContainer.evaluate((element) => element.clientWidth),
  )
  await expect(page.getByRole('button', { name: 'Clear filters' })).toBeVisible()
  await expect(page.locator('#provider-model-search-desktop')).toBeVisible()
  await expect(modelTable.locator('input')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Show filter menu for Model availability' })).toBeVisible()
  await expect
    .poll(() =>
      modelTable
        .getByRole('columnheader')
        .first()
        .evaluate((header) => Math.round(header.getBoundingClientRect().height)),
    )
    .toBeLessThanOrEqual(56)
  await expect(page.getByRole('row').filter({ hasText: /GPT Test.*openai\/gpt-test/ })).toBeVisible()
  await expect(page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })).toHaveCount(0)
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveCount(0)
  await expect(page.getByText('Use synced status')).toBeHidden()
  await page
    .getByRole('row')
    .filter({ hasText: /GPT Test.*openai\/gpt-test/ })
    .getByRole('cell')
    .filter({ hasText: /GPT Test.*openai\/gpt-test/ })
    .click()
  await expect(page).toHaveURL(/view=models&model=openai%2Fgpt-test/)
  await page.getByRole('button', { name: 'Model actions' }).click()
  await page.getByRole('menuitem', { name: 'Restore details from provider…' }).click()
  await expect(
    page.getByText(
      'This catalog entry is no longer available. Your saved model is unchanged. Refresh the catalog or update it manually.',
    ),
  ).toBeVisible()
  await expect(page.getByText('openai/gpt-test', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel', exact: true }).press('Enter')
  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  let availabilityFilterDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityFilterDialog.locator('[data-slot="select-trigger"][aria-label="Model availability"]').click()
  await page.getByRole('option', { name: 'All models', exact: true }).click()
  await expect(page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })).toHaveCount(0)
  await availabilityFilterDialog.getByRole('button', { name: 'Apply' }).click()
  await expect(modelTable.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })).toBeVisible()
  const modelColumns = await modelTable.evaluate((table) => {
    const starts = (columns: Element[]) =>
      columns.slice(0, 4).map((column) => Math.round(column.getBoundingClientRect().left))
    const rows = Array.from(table.querySelectorAll('tbody tr')).filter((row) =>
      /GPT Test.*openai\/gpt-test|Retired Model.*retired-model/s.test(row.textContent ?? ''),
    )
    return {
      header: starts(Array.from(table.querySelectorAll('thead th'))),
      rows: rows.map((row) => starts(Array.from(row.children))),
    }
  })
  expect(modelColumns.rows).toEqual([modelColumns.header, modelColumns.header])
  await expect(page.getByRole('link', { name: 'Linked models' })).toBeVisible()
  await page.getByRole('button', { name: 'Sync models' }).click()
  await expect(page.locator('[data-sonner-toast]').getByRole('button', { name: 'Retry', exact: true })).toBeVisible()
  await expect(modelTable.getByRole('row').filter({ hasText: /GPT Test.*openai\/gpt-test/ })).toBeVisible()
  await expect(modelTable.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })).toBeVisible()

  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  availabilityFilterDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityFilterDialog.locator('[data-slot="select-trigger"][aria-label="Model availability"]').click()
  await page.getByRole('option', { name: 'Unavailable', exact: true }).click()
  await availabilityFilterDialog.getByRole('button', { name: 'Apply' }).click()
  await page.locator('#provider-model-search-desktop').fill('retired')
  const retiredModelRow = page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })
  await expect(retiredModelRow).toBeVisible()
  await expect(page.getByRole('link', { name: 'Used by 1 model', exact: true })).toBeVisible()

  await retiredModelRow
    .getByRole('cell')
    .filter({ hasText: /Retired Model.*retired-model/ })
    .click()
  await expect(page).toHaveURL(/view=models&model=retired-model/)
  await expect(page.getByRole('heading', { name: 'Retired Model', level: 2 })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Save model' })).toBeVisible()
  await page.locator('#provider-model-name').fill('Unsaved name')
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(page.getByRole('alertdialog', { name: 'Discard unsaved model changes?' })).toBeVisible()
  await page.getByRole('button', { name: 'Keep editing' }).click()
  await expect(page.locator('#provider-model-name')).toHaveValue('Unsaved name')

  await page.getByLabel('Availability when adding models').click()
  await page.getByRole('option', { name: "Don't allow" }).click()
  await expect.poll(() => selectionBody?.policy).toBe('force_disabled')
  await expect(page.locator('#provider-model-name')).toHaveValue('Unsaved name')
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await page.getByRole('button', { name: 'Discard changes' }).click()
  await page
    .getByRole('row')
    .filter({ hasText: /Retired Model.*retired-model/ })
    .getByRole('cell')
    .filter({ hasText: /Retired Model.*retired-model/ })
    .click()
  await page.getByRole('button', { name: 'Model actions' }).click()
  await page.getByRole('menuitem', { name: 'Remove manually added model…' }).click()
  await expect(page.getByRole('alertdialog', { name: 'Delete Retired Model?' })).toBeVisible()
  await expect(page.getByRole('link', { name: /client-model.*retired-model/ })).toBeVisible()
  await page.getByRole('alertdialog', { name: 'Delete Retired Model?' }).getByRole('button', { name: 'Cancel' }).click()
  await expect(page.getByRole('alertdialog', { name: 'Delete Retired Model?' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()

  await page.getByRole('link', { name: 'Linked models' }).click()
  await expect(page.getByRole('link', { name: 'client-model' })).toBeVisible()
  await expect(page.getByText('retired-model', { exact: true })).toBeVisible()
  const references = page.getByRole('table', { name: 'Linked models' })
  await expect(references.getByRole('columnheader')).toHaveText(['Client model', 'Upstream model', 'Priority'])
  await expect(page.getByText('These models send requests to this provider')).toHaveCount(0)
  routeQueryFails = true
  await page.goto(`/providers/${provider.id}?view=models`)
  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  availabilityFilterDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityFilterDialog.locator('[data-slot="select-trigger"][aria-label="Model availability"]').click()
  await page.getByRole('option', { name: 'Unavailable', exact: true }).click()
  await availabilityFilterDialog.getByRole('button', { name: 'Apply' }).click()
  await page
    .getByRole('row')
    .filter({ hasText: /Retired Model.*retired-model/ })
    .getByRole('cell')
    .filter({ hasText: /Retired Model.*retired-model/ })
    .click()
  await page.getByRole('button', { name: 'Model actions' }).click()
  await page.getByRole('menuitem', { name: 'Remove manually added model…' }).click()
  await expect(page.getByText('Stravia could not check where this model is used.')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Remove from list' })).toBeDisabled()
})

test('dependency previews block referenced Provider deletion and explain Route deletion effects', async ({ page }) => {
  const provider = {
    id: 'provider-used',
    name: 'Used Provider',
    vendor: 'openai-compatible',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://used.example/v1',
    use_proxy: false,
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  const safeProvider = { ...provider, id: 'provider-safe', name: 'Safe Provider' }
  const routeModel = {
    id: 'route-used',
    model_id: 'used-route',
    display_name: null,
    balance: 'priority',
    target_provider: provider.id,
    target_model: 'upstream',
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    targets: [
      {
        id: 'target-used',
        model_id: 'route-used',
        provider_id: provider.id,
        model: 'upstream',
        weight: 100,
        priority: 1,
        created_at: '2026-08-17T00:00:00Z',
      },
    ],
  }
  let providerDeletes = 0

  await page.route('**/api/v1/providers**', async (route) => {
    if (route.request().method() === 'DELETE') providerDeletes += 1
    await route.fulfill({ json: { data: route.request().method() === 'GET' ? [provider, safeProvider] : provider } })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [routeModel] } })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'key-used',
            key: 'sk-test',
            name: 'Production key',
            is_enabled: true,
            mcp_access_enabled: false,
            transparent_injection_enabled: false,
            inject_media_understanding: false,
            inject_web_search: false,
            created_at: '2026-08-17T00:00:00Z',
            updated_at: '2026-08-17T00:00:00Z',
            model_ids: [routeModel.id],
          },
        ],
      },
    })
  })

  await page.goto('/providers')
  await page.getByRole('button', { name: `More actions for ${provider.name}` }).click()

  await page.getByRole('menuitem', { name: 'Delete provider…' }).click()
  await expect(page.getByRole('alertdialog', { name: `Cannot delete ${provider.name}` })).toBeVisible()
  await expect(page.getByRole('link', { name: /used-route.*upstream/ })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Delete provider' })).toBeDisabled()
  expect(providerDeletes).toBe(0)
  await page.getByRole('button', { name: 'Cancel' }).click()
  await page.getByRole('button', { name: `More actions for ${safeProvider.name}` }).click()
  await page.getByRole('menuitem', { name: 'Delete provider…' }).click()
  await expect(page.getByRole('alertdialog', { name: `Delete ${safeProvider.name}?` })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Delete provider' })).toBeEnabled()
  await page.getByRole('button', { name: 'Delete provider' }).click()
  await expect.poll(() => providerDeletes).toBe(1)

  await page.goto('/models')
  await page.getByRole('button', { name: `More actions for ${routeModel.model_id}` }).click()
  await page.getByRole('menuitem', { name: 'Delete model…' }).click()
  await expect(page.getByRole('alertdialog', { name: `Delete ${routeModel.model_id}?` })).toBeVisible()
  await expect(page.getByText('1 API Key permission will be removed')).toBeVisible()
  await expect(page.getByText('Production key', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel' }).click()
})

test('creating a Provider opens its saved detail and recovers automatic model sync', async ({ page }) => {
  const createdProvider = {
    id: 'created-provider',
    name: 'Created Provider',
    vendor: 'openai-compatible',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://created.example/v1',
    use_proxy: false,
    vendor_options: {},
    configured_credential_fields: ['apiKey'],
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  let created = false
  let createBody: Record<string, unknown> | undefined
  let syncCalls = 0
  const initialSync = Promise.withResolvers<void>()
  const savedSummary = {
    checked_at: '2026-10-12T10:00:00.000Z',
    source: 'manual_sync',
    record_id: 'created-sync',
    added: 3,
    missing: 1,
    restored: 2,
    deprecated: 0,
    deprecation_cleared: 0,
    specification_updated: 0,
    outcome: 'success',
    error: null,
  }
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: created ? [createdProvider] : [] } })
      return
    }
    if (path === '/providers' && request.method() === 'POST') {
      createBody = request.postDataJSON()
      created = true
      await route.fulfill({ json: { data: createdProvider } })
      return
    }
    if (path === `/providers/${createdProvider.id}/models/sync` && request.method() === 'POST') {
      syncCalls += 1
      if (syncCalls === 1) {
        await initialSync.promise
        await route.fulfill({ status: 502, json: { error: 'The model service did not respond. Try syncing again.' } })
        return
      }
      await route.fulfill({ json: { data: savedSummary } })
      return
    }
    if (path === `/providers/${createdProvider.id}/models/sync-state`) {
      await route.fulfill({
        json: {
          data: {
            automatic_sync: true,
            in_progress: false,
            pause_reason: null,
            last_check: syncCalls > 1 ? savedSummary : null,
            has_successful_sync: syncCalls > 1,
          },
        },
      })
      return
    }
    if (path === `/providers/${createdProvider.id}/models` && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models: [] } } })
      return
    }
    await route.fallback()
  })

  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await page.getByRole('button', { name: /^OpenAI Compatible / }).click()
  await page.getByLabel('Connection name').fill(createdProvider.name)
  await page.getByLabel('Base URL').fill(createdProvider.base_url)
  await page.getByLabel('API key', { exact: true }).fill('fixture-only-provider-secret')
  await page.getByRole('button', { name: 'Connect', exact: true }).click()

  await expect
    .poll(() => createBody)
    .toMatchObject({
      name: createdProvider.name,
      source: {
        type: 'custom',
        vendor: 'openai-compatible',
        channel: 'default',
        protocol: 'openai-compatible',
        base_url: createdProvider.base_url,
      },
      credential: { type: 'fields', values: { apiKey: 'fixture-only-provider-secret' } },
    })
  await expect(page).toHaveURL(new RegExp(`/providers/${createdProvider.id}\\?view=models`))
  await expect(page.getByRole('heading', { name: createdProvider.name })).toBeVisible()
  await expect(page.getByText('Syncing models…')).toBeVisible()
  await expect(page.getByRole('link', { name: 'New model from this provider' })).toHaveAttribute(
    'href',
    `/models/new?provider=${createdProvider.id}`,
  )

  initialSync.resolve()
  await expect(page.getByText("Connection saved, but models couldn't be synced")).toBeVisible()
  await page.getByRole('button', { name: 'Try again' }).click()
  const syncToast = page.locator('[data-sonner-toast]').filter({ hasText: 'Model changes saved' })
  await expect(syncToast).toBeVisible()
  await expect(syncToast).toContainText('Added 3 · No longer offered 1 · Offered again 2')
  await expect(page.locator('.route-page').getByText('Model changes saved', { exact: true })).toHaveCount(0)
  const freshness = page.locator('p').filter({ hasText: 'Last checked' })
  await expect(freshness).toBeVisible()
  await expect(freshness).toContainText('Added 3')
  await expect(freshness).toContainText('No longer offered 1')
  await expect(freshness).toContainText('Offered again 2')
  await expect.poll(() => syncCalls).toBe(2)
})

test('visible provider model actions bind exact IDs and keep inventory open', async ({ page }) => {
  const provider = {
    id: 'provider-primary',
    name: 'Primary Provider',
    vendor: 'openai-compatible',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://primary.example/v1',
    use_proxy: false,
    is_enabled: true,
    created_at: '2026-08-21T00:00:00Z',
    updated_at: '2026-08-21T00:00:00Z',
  }
  const providerModels = [
    {
      id: 'gpt-existing',
      name: 'GPT Existing',
      available: true,
      source_kind: 'discovered',
      snapshot_state: { type: 'imported', source: { type: 'discovery' } },
      selection_policy: 'auto',
      specification: { limit: { context: 128000 }, modalities: null },
      revision: 1,
    },
    {
      id: 'gpt-new',
      name: 'GPT New',
      available: true,
      source_kind: 'discovered',
      snapshot_state: { type: 'imported', source: { type: 'discovery' } },
      selection_policy: 'auto',
      specification: { limit: { context: 128000 }, modalities: null },
      revision: 1,
    },
  ]
  let routes = [
    {
      id: 'route-existing',
      model_id: 'gpt-existing',
      display_name: null,
      balance: 'weighted',
      target_provider: 'provider-secondary',
      target_model: 'gpt-existing',
      is_enabled: true,
      created_at: '2026-08-21T00:00:00Z',
      targets: [
        {
          id: 'target-secondary',
          model_id: 'route-existing',
          provider_id: 'provider-secondary',
          model: 'gpt-existing',
          weight: 100,
          priority: 1,
          created_at: '2026-08-21T00:00:00Z',
        },
      ],
    },
    {
      id: 'route-near-match',
      model_id: 'GPT-NEW',
      display_name: null,
      balance: 'weighted',
      target_provider: 'provider-secondary',
      target_model: 'gpt-new',
      is_enabled: true,
      created_at: '2026-08-21T00:00:00Z',
      targets: [
        {
          id: 'target-near-match',
          model_id: 'route-near-match',
          provider_id: 'provider-secondary',
          model: 'gpt-new',
          weight: 100,
          priority: 1,
          created_at: '2026-08-21T00:00:00Z',
        },
      ],
    },
  ]
  const bindBodies: Record<string, unknown>[] = []

  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [provider] } })
      return
    }
    if (path === `/providers/${provider.id}/models` && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models: providerModels } } })
      return
    }
    await route.fallback()
  })
  await page.route('**/api/v1/models**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/models' && request.method() === 'GET') {
      await route.fulfill({ json: { data: routes } })
      return
    }
    if (path === '/models/bind' && request.method() === 'POST') {
      const input = request.postDataJSON() as Record<string, unknown>
      bindBodies.push(input)
      if (input.provider_model_id === 'gpt-existing') {
        routes = [
          {
            ...routes[0],
            targets: [
              ...routes[0].targets,
              {
                id: 'target-primary',
                model_id: 'route-existing',
                provider_id: provider.id,
                model: 'gpt-existing',
                weight: 100,
                priority: 2,
                created_at: '2026-08-21T00:00:00Z',
              },
            ],
          },
          ...routes.slice(1),
        ]
        await route.fulfill({ json: { data: routes[0] } })
        return
      }
      const created = {
        id: 'route-new',
        model_id: 'gpt-new',
        display_name: providerModels[1].name,
        balance: 'weighted',
        target_provider: provider.id,
        target_model: 'gpt-new',
        is_enabled: true,
        created_at: '2026-08-21T00:00:00Z',
        targets: [
          {
            id: 'target-new',
            model_id: 'route-new',
            created_at: '2026-08-21T00:00:00Z',
            provider_id: provider.id,
            model: 'gpt-new',
            weight: 100,
            priority: 1,
          },
        ],
      }
      routes = [...routes, created]
      await route.fulfill({ json: { data: created } })
      return
    }
    await route.fallback()
  })

  await page.goto(`/providers/${provider.id}?view=models`)
  const existingAction = page.getByRole('button', { name: 'Add gpt-existing to a model' })
  await expect(existingAction).toHaveText('Add to existing model')

  await existingAction.click()
  const existingToast = page
    .locator('[data-sonner-toast]')
    .filter({ hasText: 'Added this provider to model gpt-existing.' })
  await expect(existingToast).toBeVisible()
  await expect(page.locator('.route-page').getByText('Added this provider to model gpt-existing.')).toHaveCount(0)
  expect(bindBodies[0]).toEqual({ provider_id: provider.id, provider_model_id: 'gpt-existing' })

  const newAction = page.getByRole('button', { name: 'Add gpt-new to a model' })
  await expect(newAction).toHaveText('Add model')
  await newAction.click()
  const createdToast = page.locator('[data-sonner-toast]').filter({ hasText: 'Created model gpt-new.' })
  await expect(createdToast).toBeVisible()
  await expect(page.locator('.route-page').getByText('Created model gpt-new.')).toHaveCount(0)
  await expect(page).toHaveURL(`/providers/${provider.id}?view=models`)
  expect(bindBodies[1]).toEqual({ provider_id: provider.id, provider_model_id: 'gpt-new' })

  await page.goto('/models')
  const modelsTable = page.getByRole('table')
  await expect(modelsTable.getByText('GPT New', { exact: true })).toBeVisible()
  await expect(modelsTable.getByText('gpt-new', { exact: true })).toBeVisible()
})

const listProviders = [
  {
    id: 'zeta-provider',
    name: 'Zeta Service',
    vendor: 'openai',
    protocol: 'openai-compatible',
    base_url: 'https://zeta.example/v1',
    use_proxy: false,
    channel: 'default',
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  },
  {
    id: 'alpha-provider',
    name: 'Alpha Service',
    vendor: 'openai-codex',
    protocol: 'open-responses',
    base_url: 'https://alpha.example/v1',
    use_proxy: false,
    channel: 'codex',
    oauth_status: 'disconnected',
    configured_credential_fields: [],
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  },
]

for (const width of [390, 320]) {
  test(`narrow Provider list is searchable by name and vendor at ${width}px`, async ({ page }) => {
    await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: listProviders } }))
    await page.setViewportSize({ width, height: 800 })
    await page.goto('/providers')

    const list = page.locator('.route-mobile-list')
    const search = page.getByRole('textbox', { name: 'Search providers…' })
    await expect(search).toBeVisible()
    const box = (await search.boundingBox())!
    expect(box.x).toBeGreaterThanOrEqual(0)
    expect(box.x + box.width).toBeLessThanOrEqual(width)
    await expect(list.getByText('Zeta Service')).toBeVisible()
    await expect(list.getByText('Alpha Service')).toBeVisible()

    await search.fill('zeta')
    await expect(list.getByText('Zeta Service')).toBeVisible()
    await expect(list.getByText('Alpha Service')).toHaveCount(0)

    // 技术名不在行内常驻，但搜索仍然能命中。
    await search.fill('openai-codex')
    await expect(list.getByText('Alpha Service')).toBeVisible()
    await expect(list.getByText('Zeta Service')).toHaveCount(0)
    await expect(list.getByText('openai-codex', { exact: true })).toHaveCount(0)

    await search.fill('__missing__')
    await expect(list.getByText('No providers match your search.')).toBeVisible()
    await search.fill('')
    await expect(list.getByText('Zeta Service')).toBeVisible()
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}

test('Provider list at 2560px is 1920px wide, keeps filling the remaining height, and hides vendor names', async ({
  page,
}) => {
  const many = Array.from({ length: 40 }, (_, index) => ({
    ...listProviders[0],
    id: `provider-${index}`,
    name: `Provider ${index}`,
  }))
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: many } }))
  await page.setViewportSize({ width: 2560, height: 1440 })
  await page.goto('/providers')
  const table = page.getByRole('table', { name: 'Connected AI providers' })
  await expect(table.getByRole('row').nth(1)).toBeVisible()
  const viewport = page.locator('[data-slot="data-table-viewport"]')
  const container = page.getByRole('main').locator('> div')
  await expect.poll(async () => Math.round((await container.boundingBox())!.width)).toBe(1920)
  expect(Math.round((await viewport.boundingBox())!.width)).toBeGreaterThan(1800)
  const bottom = await viewport.evaluate((element) => element.getBoundingClientRect().bottom)
  expect(bottom).toBeGreaterThan(1440 - 120)
  expect(await page.evaluate(() => document.documentElement.scrollHeight <= innerHeight + 1)).toBe(true)
  await expect(table.getByText('openai-codex', { exact: true })).toHaveCount(0)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})

for (const [locale, label, action] of [
  ['en-US', 'Disconnected', 'Reauthorize'],
  ['zh-CN', '已断开', '重新授权'],
] as const) {
  test(`OAuth Provider state is a localized recoverable credential state (${locale})`, async ({ page }) => {
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: listProviders } }))
    await page.goto('/providers')
    const row = page.getByRole('row').filter({ hasText: 'Alpha Service' })
    await expect(row.getByText(label)).toBeVisible()
    // 本地化状态只出现一次，原始状态枚举不会额外泄露。
    await expect(row.getByText(label, { exact: true })).toHaveCount(1)
    await expect(row.getByText(/OAuth ·/)).toHaveCount(0)
    await expect(row.getByRole('button', { name: action, exact: true })).toBeVisible()
    // 其余行没有 OAuth 状态，不出现该提示。
    await expect(page.getByRole('row').filter({ hasText: 'Zeta Service' }).getByText(label)).toHaveCount(0)
  })
}

test('Providers empty state shows one primary action centered in the remaining area', async ({ page }) => {
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [] } }))
  for (const [width, height] of [
    [1440, 900],
    [390, 844],
  ]) {
    await page.setViewportSize({ width, height })
    await page.goto('/providers')
    const action = page.getByRole('button', { name: 'Add your first provider' })
    await expect(action).toBeVisible()
    await expect(page.getByText('A model has nowhere to go until you connect a provider.')).toHaveCount(0)
    await expect(page.getByRole('button', { name: /Add provider/ })).toHaveCount(0)
    const empty = page.locator('[data-slot="empty"]')
    const box = (await empty.boundingBox())!
    const center = box.y + box.height / 2
    expect(center).toBeGreaterThan(height * 0.35)
    expect(center).toBeLessThan(height * 0.65)
  }
})

test('Add provider Sheet keeps one width across both steps and uses a flat proxy row', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/providers')
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await expect(page.getByRole('tab', { name: 'Choose provider' })).toHaveAttribute('aria-selected', 'true')
  // 滑入动画结束后量取（x 稳定在 1440 - 896）。
  await expect.poll(async () => Math.round((await sheet.boundingBox())!.x)).toBe(544)
  const picker = (await sheet.boundingBox())!
  expect(Math.round(picker.width)).toBe(896)
  await page.getByRole('button', { name: /^OpenAI Compatible / }).click()
  await expect(page.getByRole('tab', { name: 'Connection details' })).toHaveAttribute('aria-selected', 'true')
  const configure = (await sheet.boundingBox())!
  expect(Math.round(configure.width)).toBe(896)
  expect(Math.round(configure.x)).toBe(Math.round(picker.x))
  // 代理是草稿的一部分，扁平设置行：没有外框。
  const proxyRow = page.locator('[data-slot="setting-row"]').filter({ has: page.getByRole('switch') })
  await expect(proxyRow).toHaveCount(1)
  expect(await proxyRow.evaluate((row) => getComputedStyle(row).borderTopWidth)).toBe('0px')
  await expect(page.getByRole('switch', { name: 'Use proxy' })).toBeVisible()
})

test('Add provider draft asks before every close path', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/providers')
  const dialog = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })
  const heading = page.getByRole('heading', { name: 'Connection details' })
  const open = async () => {
    await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
    await page.getByRole('button', { name: /^OpenAI Compatible / }).click()
    await expect(heading).toBeVisible()
  }

  // 没有改动：每个出口直接关闭。
  await open()
  await page.keyboard.press('Escape')
  await expect(heading).toHaveCount(0)
  await expect(dialog).toHaveCount(0)

  // 有草稿：Escape、关闭按钮、取消、返回、切换步骤都先确认。
  await open()
  await page.getByLabel('Base URL', { exact: true }).fill('https://draft.example/v1')
  const paths: Array<[string, () => Promise<void>]> = [
    ['Escape', () => page.keyboard.press('Escape')],
    ['close button', () => page.getByRole('button', { name: 'Close provider setup' }).click()],
    ['Cancel', () => page.getByRole('button', { name: 'Cancel', exact: true }).click()],
    ['Back', () => page.getByRole('button', { name: 'Back', exact: true }).click()],
    ['Choose provider tab', () => page.getByRole('tab', { name: 'Choose provider' }).click()],
  ]
  for (const [name, leave] of paths) {
    await leave()
    await expect(dialog, name).toBeVisible()
    await dialog.getByRole('button', { name: 'Keep editing' }).click()
    await expect(dialog, name).toHaveCount(0)
    await expect(heading, name).toBeVisible()
    await expect(page.getByLabel('Base URL', { exact: true }), name).toHaveValue('https://draft.example/v1')
  }

  // 确认放弃后关闭，重新打开从选择服务开始且草稿已清空。
  await page.keyboard.press('Escape')
  await dialog.getByRole('button', { name: 'Discard changes' }).click()
  await expect(heading).toHaveCount(0)
  await page.getByRole('button', { name: /Add (your first )?provider/ }).click()
  await expect(page.getByRole('tab', { name: 'Choose provider' })).toHaveAttribute('aria-selected', 'true')
  await page.getByRole('button', { name: /^OpenAI Compatible / }).click()
  await expect(page.getByLabel('Base URL', { exact: true })).not.toHaveValue('https://draft.example/v1')
})

const detailTimestamp = '2026-01-01T00:00:00Z'

function detailProvider(id: string, overrides: Record<string, unknown> = {}) {
  return {
    id,
    name: `Provider ${id}`,
    vendor: 'openai',
    channel: 'default',
    protocol: 'openai-compatible',
    base_url: 'https://fixture.invalid/v1',
    use_proxy: false,
    vendor_options: {},
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    created_at: detailTimestamp,
    updated_at: detailTimestamp,
    ...overrides,
  }
}

async function mockProviderDetail(
  page: Page,
  providers: Array<ReturnType<typeof detailProvider>>,
  options: { updates?: unknown[]; models?: unknown[]; routes?: unknown[]; syncState?: unknown } = {},
) {
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: options.routes ?? [] } }))
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: providers } })
    } else if (/^\/providers\/[^/]+\/models$/.test(path) && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models: options.models ?? [] } } })
    } else if (path.endsWith('/models/sync-state') && request.method() === 'GET') {
      if (options.syncState) await route.fulfill({ json: { data: options.syncState } })
      else await route.fulfill({ status: 404, json: { error: 'not found' } })
    } else if (/^\/providers\/[^/]+$/.test(path) && request.method() === 'PUT') {
      options.updates?.push(request.postDataJSON())
      await route.fulfill({ json: { data: providers.find((item) => path.endsWith(item.id)) } })
    } else {
      await route.fallback()
    }
  })
}

test('Provider connection draft confirms before tab switch, route leave and reload, and Cancel restores saved values', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  const provider = detailProvider('draft-provider', { name: 'Saved name' })
  const updates: unknown[] = []
  await mockProviderDetail(page, [provider], { updates })
  await page.goto('/providers/draft-provider?view=connection')

  const name = page.getByLabel('Connection name')
  const cancel = page.getByRole('button', { name: 'Cancel', exact: true })
  const dialog = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })
  const tabs = page.getByRole('navigation', { name: 'Provider details' })
  await expect(name).toHaveValue('Saved name')

  // 没有改动：Cancel 不可用，切换页签与刷新都不打扰。
  await expect(cancel).toBeDisabled()
  const cleanUnload = await page.evaluate(() => {
    const event = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(event)
    return event.defaultPrevented
  })
  expect(cleanUnload).toBe(false)

  await name.fill('Edited name')
  await expect(cancel).toBeEnabled()

  // 刷新：有未保存改动时交给浏览器的原生离开提示。
  const dirtyUnload = await page.evaluate(() => {
    const event = new Event('beforeunload', { cancelable: true })
    window.dispatchEvent(event)
    return event.defaultPrevented
  })
  expect(dirtyUnload).toBe(true)

  // 切换页签。
  await tabs.getByRole('link', { name: 'Linked models' }).click()
  await expect(dialog).toBeVisible()
  await expect(page).toHaveURL(/view=connection/)
  await dialog.getByRole('button', { name: 'Keep editing' }).click()
  await expect(dialog).toBeHidden()
  await expect(name).toHaveValue('Edited name')

  // 路由离开。
  await page.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Providers' }).click()
  await expect(dialog).toBeVisible()
  await expect(page).toHaveURL(/\/providers\/draft-provider/)
  await dialog.getByRole('button', { name: 'Keep editing' }).click()
  await expect(name).toHaveValue('Edited name')

  // Cancel 同样经过确认，放弃后回到已保存的值。
  await cancel.click()
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Keep editing' }).click()
  await expect(name).toHaveValue('Edited name')
  await cancel.click()
  await dialog.getByRole('button', { name: 'Discard changes' }).click()
  await expect(name).toHaveValue('Saved name')
  await expect(cancel).toBeDisabled()

  // 代理开关属于草稿：改动后离开同样确认，放弃后不产生任何保存请求。
  await page.getByRole('switch', { name: 'Use proxy' }).click()
  await expect(cancel).toBeEnabled()
  await tabs.getByRole('link', { name: 'Linked models' }).click()
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/view=routes/)
  expect(updates).toEqual([])

  // 放弃后回到连接页：显示已保存的值，切换页签不再确认。
  await tabs.getByRole('link', { name: 'Connection settings' }).click()
  await expect(page.getByRole('switch', { name: 'Use proxy' })).not.toBeChecked()
  await expect(page.getByLabel('Connection name')).toHaveValue('Saved name')
  await tabs.getByRole('link', { name: 'Linked models' }).click()
  await expect(page).toHaveURL(/view=routes/)
  await expect(dialog).toHaveCount(0)
})

test('Provider connection actions stay in view and lock while saving, and a saved draft leaves without confirmation', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 500 })
  const provider = detailProvider('lock-provider', {
    vendor: 'openai-compatible',
    configured_credential_fields: ['apiKey'],
  })
  const { promise: gate, resolve: release } = Promise.withResolvers<void>()
  const updates: unknown[] = []
  await mockProviderDetail(page, [provider])
  await page.route('**/api/v1/providers/lock-provider', async (route) => {
    if (route.request().method() !== 'PUT') return route.fallback()
    updates.push(route.request().postDataJSON())
    await gate
    await route.fulfill({ json: { data: { ...provider, name: 'Saved once' } } })
  })
  await page.goto('/providers/lock-provider?view=connection')
  const name = page.getByLabel('Connection name')
  const save = page.getByRole('button', { name: 'Save connection' })
  const cancel = page.getByRole('button', { name: 'Cancel', exact: true })
  await name.fill('Saved once')

  // 固定操作区在视口内，取消在前、保存在后。
  const [cancelBox, saveBox] = [(await cancel.boundingBox())!, (await save.boundingBox())!]
  expect(saveBox.y + saveBox.height).toBeLessThanOrEqual(500)
  expect(cancelBox.x).toBeLessThan(saveBox.x)

  await save.click()
  await expect(name).toBeDisabled()
  await expect(cancel).toBeDisabled()
  await expect(save).toBeDisabled()
  // 保存中路由离开被锁定，且不弹出确认。
  await page.getByRole('navigation', { name: 'Provider details' }).getByRole('link', { name: 'Linked models' }).click()
  await expect(page).toHaveURL(/view=connection/)
  await expect(page.getByRole('alertdialog')).toHaveCount(0)
  release()
  await expect(name).toBeEnabled()
  expect(updates).toHaveLength(1)

  // 保存成功后草稿即为基线：离开不再确认。
  await page.getByRole('navigation', { name: 'Provider details' }).getByRole('link', { name: 'Linked models' }).click()
  await expect(page).toHaveURL(/view=routes/)
  await expect(page.getByRole('alertdialog')).toHaveCount(0)
})

test('Provider connection form keeps one right edge at 60rem and shows the side column only with real status', async ({
  page,
}) => {
  await page.setViewportSize({ width: 2560, height: 1440 })
  const withStatus = detailProvider('status-provider', {
    vendor: 'openai-compatible',
    configured_credential_fields: [],
    credential_status: 'invalid',
    credential_invalid_at: '2026-10-01T10:00:00Z',
    allowance_suspension: { suspended_at: '2026-09-01T12:00:00Z', triggered_keys: ['weekly'], earliest_reset_at: null },
  })
  const clean = detailProvider('clean-provider')
  await mockProviderDetail(page, [withStatus, clean], {
    syncState: {
      automatic_sync: true,
      in_progress: false,
      pause_reason: null,
      has_successful_sync: true,
      last_check: {
        checked_at: '2026-10-12T10:00:00.000Z',
        source: 'manual_sync',
        record_id: 'record',
        added: 2,
        missing: 0,
        restored: 0,
        deprecated: 0,
        deprecation_cleared: 0,
        specification_updated: 0,
        outcome: 'success',
        error: null,
      },
    },
  })

  await page.goto('/providers/status-provider?view=connection')
  const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Save connection' }) })
  const controls = [
    page.getByLabel('Connection name'),
    page.getByLabel('Base URL'),
    page.locator('[data-slot="input-group"]').filter({ has: page.getByLabel('API key', { exact: true }) }),
  ]
  const labelBox = (await page.locator('label[for="provider-name"]').boundingBox())!
  const boxes = await Promise.all(controls.map(async (control) => (await control.boundingBox())!))
  const formBox = (await form.boundingBox())!
  expect(formBox.width).toBeLessThanOrEqual(960)
  // 名称、地址、密钥同宽、同一左右边缘；label 到控件右缘的距离受 60rem 约束。
  for (const box of boxes) {
    expect(Math.abs(box.x - boxes[0].x)).toBeLessThanOrEqual(1)
    expect(Math.abs(box.width - boxes[0].width)).toBeLessThanOrEqual(1)
  }
  expect(boxes[0].width).toBeLessThanOrEqual(576 + 1)
  expect(boxes[0].x + boxes[0].width - labelBox.x).toBeLessThanOrEqual(960)
  expect(boxes[0].x + boxes[0].width).toBeLessThanOrEqual(formBox.x + formBox.width + 1)

  // 说明只进问号：复述 label 的说明省略，真正的数据流向说明保留在问号里。
  const apiKeyField = page.locator('[data-slot="field"]').filter({ has: page.getByLabel('API key', { exact: true }) })
  await expect(apiKeyField.locator('[data-slot="field-hint"]')).toHaveCount(1)
  await expect(page.getByText('Credential sent to the configured endpoint.')).toBeHidden()
  await apiKeyField.locator('[data-slot="field-hint"]').hover()
  await expect(page.locator('[data-slot="tooltip-content"]').first()).toContainText(
    'Credential sent to the configured endpoint.',
  )
  const proxyRow = page
    .locator('[data-slot="setting-row"]')
    .filter({ has: page.getByRole('switch', { name: 'Use proxy' }) })
  // 代理数据流向是常驻 Description，不再进问号。
  await expect(proxyRow).toContainText('Send requests to this service through the proxy configured in Settings.')
  await expect(proxyRow.locator('[data-slot="field-hint"]')).toHaveCount(0)

  // 辅助栏：只含真实状态，位于表单右侧、与表单顶部对齐。
  const aside = page.getByRole('complementary', { name: 'Connection status' })
  await expect(aside).toBeVisible()
  await expect(aside).toContainText('Rejected by the provider since')
  await expect(aside).toContainText('Paused since')
  await expect(aside).toContainText('weekly')
  await expect(aside).toContainText('Last model sync')
  const asideBox = (await aside.boundingBox())!
  expect(asideBox.x).toBeGreaterThanOrEqual(formBox.x + formBox.width)
  expect(Math.abs(asideBox.y - formBox.y)).toBeLessThanOrEqual(40)

  // 没有任何真实状态时不显示辅助栏。
  await page.route('**/api/v1/providers/clean-provider/models/sync-state', (route) =>
    route.fulfill({ status: 404, json: { error: 'not found' } }),
  )
  await page.goto('/providers/clean-provider?view=connection')
  await expect(page.getByRole('button', { name: 'Save connection' })).toBeVisible()
  await expect(page.getByRole('complementary', { name: 'Connection status' })).toHaveCount(0)
})

test('Provider upstream model table fills the 120rem container at 2560', async ({ page }) => {
  await page.setViewportSize({ width: 2560, height: 1440 })
  const provider = detailProvider('wide-provider')
  await mockProviderDetail(page, [provider], {
    models: [
      {
        id: 'wide-model',
        name: 'Wide Model',
        available: true,
        source_kind: 'discovered',
        snapshot_state: { type: 'imported', source: { type: 'discovery' } },
        selection_policy: 'auto',
        specification: { limit: { context: 128000 }, modalities: null },
        revision: 1,
      },
    ],
  })
  await page.goto('/providers/wide-provider?view=models')
  const table = page.getByRole('table', { name: 'Models from this provider' })
  await expect(table).toBeVisible()
  const container = page.locator('.route-page')
  const tableBox = (await table.boundingBox())!
  const containerBox = (await container.boundingBox())!
  expect(containerBox.width).toBeLessThanOrEqual(1920 + 1)
  expect(Math.abs(tableBox.width - containerBox.width)).toBeLessThanOrEqual(2)
  expect(tableBox.width).toBeGreaterThanOrEqual(1900)
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})

const compactModel = (id: string, name: string, specification: Record<string, unknown>) => ({
  id,
  name,
  available: true,
  source_kind: 'discovered',
  snapshot_state: { type: 'imported', source: { type: 'discovery' } },
  selection_policy: 'auto',
  specification,
  revision: 1,
})

for (const width of [390, 320]) {
  test(`narrow upstream model rows are compact labelled facts with visible editors (${width}px)`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 })
    const provider = detailProvider('mobile-provider')
    await mockProviderDetail(page, [provider], {
      models: [
        compactModel('mobile-a', 'Mobile A', {
          limit: { context: 128000 },
          modalities: { input: ['text', 'image'], output: ['text'] },
          reasoning_efforts: ['low', 'medium', 'high'],
        }),
        compactModel('mobile-b', 'Mobile B', { limit: null, modalities: null }),
      ],
    })
    await page.goto('/providers/mobile-provider?view=models')
    const row = page.locator('article').filter({ hasText: 'mobile-a' })
    await expect(row).toBeVisible()
    const box = (await row.boundingBox())!
    // 现状约 304px；紧凑行显著更矮。
    expect(box.height).toBeLessThan(width === 390 ? 240 : 290)

    // 带标签的 dl：每个事实都有可见的名称。
    const labels = row.locator('dt:not(.sr-only)')
    await expect(labels).toHaveText(['Context', 'RPM limit', 'Modalities', 'Reasoning effort', 'Model availability'])

    // 四类单元格的编辑入口都可见且在视口内。
    for (const name of [
      'Context tokens mobile-a',
      'Modalities mobile-a',
      'Reasoning effort mobile-a',
      'Availability when adding models mobile-a',
    ]) {
      const trigger = row.getByRole('button', { name, exact: true })
      await expect(trigger).toBeVisible()
      const triggerBox = (await trigger.boundingBox())!
      expect(triggerBox.x).toBeGreaterThanOrEqual(0)
      expect(triggerBox.x + triggerBox.width).toBeLessThanOrEqual(width)
      expect(triggerBox.height).toBeGreaterThanOrEqual(24)
    }
    await row.getByRole('button', { name: 'Context tokens mobile-a', exact: true }).click()
    await expect(page.getByRole('dialog').getByRole('spinbutton')).toBeVisible()
    await page.keyboard.press('Escape')

    // 添加／使用动作与身份同一行、靠前。
    const action = row.getByRole('button', { name: /Add .*mobile-a|Create .*mobile-a|mobile-a/ }).first()
    await expect(action).toBeVisible()
    const identity = (await row.getByRole('link', { name: /Mobile A/ }).boundingBox())!
    const actionBox = (await row
      .getByRole('button', { name: /route|model/i })
      .first()
      .boundingBox())!
    expect(actionBox.y).toBeLessThan(identity.y + identity.height)
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}

test('Linked models list labels both identities and only collapses an upstream ID identical to the client ID', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  const provider = detailProvider('linked-provider')
  const route = (id: string, modelId: string, displayName: string, upstream: string, priority: number) => ({
    id,
    model_id: modelId,
    display_name: displayName,
    balance: 'priority',
    target_provider: provider.id,
    target_model: upstream,
    is_enabled: true,
    created_at: detailTimestamp,
    targets: [
      {
        id: `target-${id}`,
        model_id: id,
        provider_id: provider.id,
        model: upstream,
        weight: 100,
        priority,
        created_at: detailTimestamp,
      },
    ],
  })
  await mockProviderDetail(page, [provider], {
    routes: [route('r-different', 'coding', 'Coding', 'gpt-5.4', 1), route('r-same', 'gpt-5.4', '', 'gpt-5.4', 2)],
  })
  await page.goto('/providers/linked-provider?view=routes')
  const list = page.getByRole('table', { name: 'Linked models' })
  await expect(list.getByRole('columnheader')).toHaveText(['Client model', 'Upstream model', 'Priority'])
  const different = list.getByRole('row').filter({ hasText: 'Coding' })
  await expect(different.getByRole('cell')).toHaveCount(3)
  await expect(different.getByRole('cell').nth(0)).toContainText('coding')
  await expect(different.getByRole('cell').nth(1)).toContainText('gpt-5.4')
  await expect(different.getByRole('cell').nth(2)).toContainText('1')
  const same = list.getByRole('row').filter({ hasText: 'Same ID' })
  await expect(same.getByRole('cell').nth(0)).toContainText('gpt-5.4')
  await expect(same.getByRole('cell').nth(1)).toContainText('Same ID')
  await expect(page.getByText('These models send requests to this provider')).toHaveCount(0)

  // 优先级紧挨着标识列，而不是被推到容器边缘。
  const [clientHeader, priorityHeader] = await Promise.all([
    list.getByRole('columnheader', { name: 'Client model' }).boundingBox(),
    list.getByRole('columnheader', { name: 'Priority' }).boundingBox(),
  ])
  expect(priorityHeader!.x - clientHeader!.x).toBeLessThan(900)

  // 窄屏：三个值各自带标签，没有表头也能读懂。
  await page.setViewportSize({ width: 390, height: 844 })
  const narrow = list.getByRole('row').filter({ hasText: 'Coding' })
  await expect(narrow.getByRole('cell').nth(0)).toContainText('Client model')
  await expect(narrow.getByRole('cell').nth(1)).toContainText('Upstream model')
  await expect(narrow.getByRole('cell').nth(2)).toContainText('Priority')
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})
