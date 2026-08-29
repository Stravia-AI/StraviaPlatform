import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
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
      auth_mode: 'apikey',
      preset_key: null,
      channel: null,
      models_source: null,
      static_models: null,
      is_enabled: true,
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    },
    {
      id: 'alpha-provider',
      name: 'Alpha Service',
      vendor: 'anthropic',
      protocol: 'anthropic-messages',
      base_url: 'https://alpha.example/v1',
      use_proxy: false,
      auth_mode: 'oauth',
      preset_key: null,
      channel: null,
      models_source: null,
      static_models: null,
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

  const table = page.getByRole('table', { name: 'Connected AI model services' })
  await expect(table.getByRole('row')).toHaveCount(3)
  const tableLayout = await table.evaluate((element) => {
    const viewport = element.closest('[data-slot="data-table-viewport"]')
    const container = element.closest('[data-slot="table-container"]')
    const bodyRows = element.querySelectorAll('[data-slot="table-body"] > [data-slot="table-row"]')
    const firstCell = bodyRows[0]?.querySelector('[data-slot="table-cell"]')
    const firstResizer = element.querySelector('button[aria-label^="Resize the"]')
    if (!(viewport instanceof HTMLElement) || !(container instanceof HTMLElement)) {
      throw new Error('DataTable layout containers are missing')
    }
    if (!(firstCell instanceof HTMLElement) || !(firstResizer instanceof HTMLElement) || bodyRows.length < 2) {
      throw new Error('DataTable style targets are missing')
    }
    return {
      unusedWidth: viewport.clientWidth - element.offsetWidth,
      horizontalOverflow: container.scrollWidth - container.clientWidth,
      viewportOverflow: getComputedStyle(viewport).overflow,
      bodyCellInlineBorderWidth: getComputedStyle(firstCell).borderInlineEndWidth,
      resizerLineColor: getComputedStyle(firstResizer, '::after').backgroundColor,
      firstRowBackground: getComputedStyle(bodyRows[0]).backgroundColor,
      secondRowBackground: getComputedStyle(bodyRows[1]).backgroundColor,
    }
  })
  expect(tableLayout.unusedWidth).toBeLessThanOrEqual(1)
  expect(tableLayout.horizontalOverflow).toBeLessThanOrEqual(1)
  expect(tableLayout.viewportOverflow).toBe('hidden')
  expect(tableLayout.bodyCellInlineBorderWidth).toBe('0px')
  expect(tableLayout.resizerLineColor).toBe('rgba(0, 0, 0, 0)')
  expect(tableLayout.firstRowBackground).toBe('rgba(0, 0, 0, 0)')
  expect(tableLayout.secondRowBackground).not.toBe('rgba(0, 0, 0, 0)')
  const search = page.getByPlaceholder('Search model services…')
  await search.fill('Alpha')
  await expect(table.getByRole('row')).toHaveCount(2)
  await expect(table).toContainText('Alpha Service')
  await expect(table).not.toContainText('Zeta Service')
  await search.fill('__missing__')
  await expect(table).toContainText('No model services match your search.')
  await search.fill('')

  await page.getByRole('button', { name: 'Columns' }).click()
  await page.getByRole('menuitemcheckbox', { name: 'Authentication' }).click()
  await expect(table.getByRole('columnheader', { name: /Authentication/ })).toHaveCount(0)

  await page.reload()
  await expect(table.getByRole('columnheader', { name: /Authentication/ })).toHaveCount(0)
  await table.getByRole('button', { name: 'Sort Model service ascending' }).click()
  await expect(table.getByRole('row').nth(1)).toContainText('Alpha Service')
})

test('provider editor selects a Provider option before showing its configuration', async ({ page }) => {
  await page.goto('/providers')
  await expect(page.getByRole('button', { name: 'Update service list' })).toHaveCount(0)
  await page.getByRole('button', { name: /Connect (first )?service/ }).click()

  await expect(page.getByRole('heading', { name: 'Connect a model service' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Connect', exact: true })).toHaveCount(0)
  const searchInput = page.getByPlaceholder('Search services')
  await expect(searchInput).toBeVisible()
  const refreshButton = page.getByRole('button', { name: 'Update service list' })
  await expect(refreshButton).toBeVisible()
  const searchBox = await searchInput.boundingBox()
  const refreshBox = await refreshButton.boundingBox()
  expect(searchBox).not.toBeNull()
  expect(refreshBox).not.toBeNull()
  expect(refreshBox!.x).toBeGreaterThan(searchBox!.x + searchBox!.width)
  const verticalOffset = Math.abs(refreshBox!.y + refreshBox!.height / 2 - (searchBox!.y + searchBox!.height / 2))
  expect(verticalOffset).toBeLessThanOrEqual(1)
  await refreshButton.click()
  await expect(page.getByText('Service list updated: 2 services and 4 models.')).toBeVisible()
  const toolbar = page.locator('[data-provider-toolbar]')
  await expect(toolbar).toHaveCSS('position', 'sticky')
  const cardWidths = await page
    .locator('[data-provider-option]')
    .evaluateAll((cards) => cards.map((card) => card.getBoundingClientRect().width))
  expect(Math.min(...cardWidths)).toBeGreaterThanOrEqual(248)
  await expect(page.getByRole('tab', { name: 'Featured' })).toHaveCount(0)
  await expect(page.getByRole('tab', { name: 'Other' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: /Custom.*Bring your own/ })).toHaveCount(1)
  await expect(page.getByRole('button', { name: /OpenAI.*API key/ })).toBeVisible()
  await expect(page.getByRole('button', { name: /Codex.*OAuth account/ })).toBeVisible()
  await expect(page.getByText(/^\d+ models$/)).toHaveCount(0)

  await searchInput.fill('OpenAI')
  await expect(page.getByRole('button', { name: /OpenAI.*API key/ })).toBeVisible()
  await expect(page.getByRole('button', { name: /Codex.*OAuth account/ })).toBeVisible()
  await expect(page.locator('img[src$="/catalog/providers/openai/logo"]').first()).toBeVisible()
  await expect(page.getByRole('tab', { name: 'Choose service' })).toHaveAttribute('data-state', 'active')
  const openAiOption = page.getByRole('button', { name: /OpenAI.*API key/ })
  const codexOption = page.getByRole('button', { name: /Codex.*OAuth account/ })
  await expect(openAiOption.getByText('API Key', { exact: true })).toHaveCount(0)
  await expect(openAiOption.getByText('API key', { exact: true })).toBeVisible()
  await expect(codexOption.getByText('OAuth', { exact: true })).toHaveCount(0)
  await expect(codexOption.getByText('Codex · OAuth account', { exact: true })).toBeVisible()
  await openAiOption.focus()
  await openAiOption.press('ArrowRight')
  await expect(codexOption).toBeFocused()

  await searchInput.fill('__missing_provider__')
  await expect(page.getByText('No matching services', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Clear search' }).click()
  await expect(page.getByText('No matching services', { exact: true })).toHaveCount(0)

  await page.getByRole('button', { name: /Custom.*Bring your own/ }).click()

  await expect(page.getByRole('heading', { name: 'Connection details' })).toBeVisible()
  await expect(page.getByLabel('Service identifier')).toBeVisible()
  await expect(page.getByLabel('Base URL')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Connect', exact: true })).toBeVisible()
  await page.getByRole('tab', { name: 'Choose service' }).click()
  await expect(page.getByRole('heading', { name: 'Connect a model service' })).toBeVisible()
})

test('configured Providers use Catalog logos and Custom uses its endpoint favicon', async ({ page }) => {
  const configuredProviders = [
    {
      id: 'xiaomi-provider',
      name: 'Xiaomi Catalog',
      vendor: 'xiaomi',
      protocol: 'openai-compatible',
      base_url: 'https://api.xiaomi.example/v1',
      use_proxy: false,
      auth_mode: 'apikey',
      preset_key: 'xiaomi',
      channel: 'default',
      models_source: 'catalog',
      static_models: null,
      is_enabled: true,
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    },
    {
      id: 'custom-provider',
      name: 'Custom Endpoint',
      vendor: 'openai',
      protocol: 'openai-compatible',
      base_url: 'https://custom.example/v1',
      use_proxy: false,
      auth_mode: 'apikey',
      preset_key: null,
      channel: null,
      models_source: null,
      static_models: null,
      is_enabled: true,
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    },
  ]
  await page.route('**/api/v1/providers', async (route) => {
    if (route.request().method() !== 'GET') {
      await route.fallback()
      return
    }
    await route.fulfill({ json: { data: configuredProviders } })
  })
  await page.route('**/api/v1/providers/image-capability-drifts', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'drift-1',
            provider_id: 'xiaomi-provider',
            upstream_model: 'mimo-v2',
            safe_message: 'Image output support changed',
            detected_at: '2026-01-02T00:00:00Z',
          },
        ],
      },
    })
  })
  await page.goto('/providers')

  const xiaomiRow = page.getByRole('row').filter({ hasText: 'Xiaomi Catalog' })
  const catalogLogo = xiaomiRow.locator('img[src$="/catalog/providers/xiaomi/logo"]')
  await expect(catalogLogo).toBeVisible()
  await page.evaluate(() => document.documentElement.classList.add('dark'))
  await expect(catalogLogo).toHaveCSS('filter', 'invert(1)')
  await expect(xiaomiRow.locator('[title*="mimo-v2"]')).toBeVisible()
  const customMobileRow = page.locator('.route-mobile-row').filter({ hasText: 'Custom Endpoint' })
  await expect(customMobileRow.locator('img')).toHaveAttribute('src', 'https://custom.example/favicon.ico')
})

test('editing a legacy Provider preserves credentials and legacy option fields', async ({ page }) => {
  const legacyProvider = {
    id: 'legacy-provider',
    name: 'Legacy Provider',
    vendor: 'openai',
    protocol: 'openai-compatible',
    base_url: 'https://legacy.example/v1',
    use_proxy: false,
    auth_mode: 'apikey',
    preset_key: null,
    channel: null,
    models_source: null,
    static_models: null,
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  let updateBody: Record<string, unknown> | undefined
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: [legacyProvider] } })
      return
    }
    if (path === '/providers/legacy-provider/models' && request.method() === 'GET') {
      await route.fulfill({
        json: {
          data: {
            models: [
              {
                id: 'test-model',
                name: 'GPT Test',
                available: true,
                source_kind: 'discovered',
                selection_policy: 'auto',
                capabilities: { tool_call: true, reasoning: true, attachment: false, context: 128000 },
                revision: 1,
              },
            ],
          },
        },
      })
      return
    }
    if (path === '/providers/legacy-provider' && request.method() === 'PUT') {
      updateBody = request.postDataJSON()
      await route.fulfill({ json: { data: legacyProvider } })
      return
    }
    await route.fallback()
  })

  await page.goto('/providers/legacy-provider?view=connection')
  await expect(page.getByRole('heading', { name: legacyProvider.name })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Available models' })).toBeVisible()
  await page.getByRole('button', { name: 'Save connection' }).click()

  await expect.poll(() => updateBody).toBeDefined()
  expect(updateBody).not.toHaveProperty('api_key')
  expect(updateBody).not.toHaveProperty('preset_key')
  expect(updateBody).not.toHaveProperty('channel')
})

test('OAuth Provider connection view reconnects the saved account without showing API Key input', async ({ page }) => {
  const provider = {
    id: 'oauth-provider',
    name: 'Codex Account',
    vendor: 'openai',
    protocol: 'open-responses',
    base_url: 'https://api.openai.com/v1',
    use_proxy: false,
    auth_mode: 'oauth',
    preset_key: 'openai',
    channel: 'codex',
    models_source: 'catalog',
    static_models: null,
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
  await expect(page.getByLabel('API Key')).toHaveCount(0)
  const initRequest = page.waitForRequest(
    (request) => request.url().endsWith('/api/v1/oauth/sessions/init') && request.method() === 'POST',
  )
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in again' }).click()
  const popup = await popupPromise
  await popup.close()
  expect((await initRequest).postDataJSON()).toMatchObject({ vendor: provider.channel, use_proxy: false })
  await expect(page.getByText('Waiting for authorization…')).toBeVisible()
  await page.getByRole('button', { name: 'Cancel sign-in' }).click()
  await expect(page.getByRole('button', { name: 'Sign in again' })).toBeVisible()
})

test('Provider Model editor uses structured fields and preserves exact decimal input', async ({ page }) => {
  const provider = {
    id: 'visual-provider',
    name: 'Visual Provider',
    vendor: 'openai',
    protocol: 'openai-compatible',
    base_url: 'https://models.example/v1',
    use_proxy: false,
    auth_mode: 'apikey',
    preset_key: 'openai',
    channel: 'default',
    models_source: 'catalog',
    static_models: null,
    is_enabled: true,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
  const summary = {
    id: 'gpt-test',
    name: 'GPT Test',
    available: true,
    source_kind: 'discovered',
    selection_policy: 'auto',
    capabilities: { tool_call: true, reasoning: true, attachment: false, context: 128000 },
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
      attachment: false,
      reasoning: true,
      tool_call: true,
      open_weights: true,
      modalities: { input: ['text', 'image', 'binary'], output: ['text'] },
      limit: { context: 128000, input: null, output: 32000 },
      cost: {
        input: 0.25,
        output: 1,
        reasoning: 2,
        input_audio: 3,
        output_audio: 4,
        context_over_200k: { input: 5, output: 22.5 },
        tiers: [
          {
            tier: { type: 'context', size: 272000 },
            input: 6,
            output: 24,
            reasoning: 7,
            input_audio: 8,
            output_audio: 9,
          },
        ],
      },
      reasoning_options: [{ type: 'effort', values: ['low', 'medium', 'high', 'future'] }],
      interleaved: { field: 'reasoning_effort' },
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
            available: true,
            source_kind: 'manual',
            selection_policy: 'auto',
            metadata: { id: body.model_id, name: body.template_id ? 'GPT-5.4' : 'Known New Model', reasoning: true },
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
  await availableModelRow.getByRole('cell').nth(1).click()

  await expect(page.locator('#provider-model-id')).toHaveValue('gpt-test')
  await expect(page.locator('#provider-model-metadata')).toHaveCount(0)
  await page.getByText('Advanced model settings', { exact: false }).click()
  await expect(page.getByText('Extension fields (read only) · 1')).toBeVisible()
  await expect(page.locator('#provider-model-family')).toHaveCount(0)
  await expect(page.locator('#provider-model-knowledge')).toHaveCount(0)
  await expect(page.locator('#provider-model-release_date')).toHaveCount(0)
  await expect(page.locator('#provider-model-last_updated')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Remove Description' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Remove Attachments' })).toHaveCount(0)
  await expect(page.locator('#provider-model-open_weights')).toHaveCount(0)
  await expect(page.getByText('Context over 200k')).toHaveCount(0)
  await expect(page.locator('#provider-model-tier-0')).toHaveValue('272000')
  await expect(page.locator('#provider-model-tier-1')).toHaveCount(0)
  await expect(page.locator('#provider-model-cost-reasoning')).toHaveCount(0)
  await expect(page.locator('#provider-model-cost-input_audio')).toHaveCount(0)
  await expect(page.locator('#provider-model-cost-output_audio')).toHaveCount(0)
  await expect(page.locator('#provider-model-tier-0-reasoning')).toHaveCount(0)
  await expect(page.locator('#provider-model-tier-0-input_audio')).toHaveCount(0)
  await expect(page.locator('#provider-model-tier-0-output_audio')).toHaveCount(0)
  const inputModalities = page.locator('[data-modality-select="input"]')
  await expect(inputModalities).toContainText('text, image, binary')
  await inputModalities.click()
  for (const value of ['text', 'image', 'audio', 'video', 'pdf', 'binary']) {
    await expect(page.getByRole('option', { name: value, exact: true })).toBeVisible()
  }
  await page.getByRole('option', { name: 'audio', exact: true }).click()
  await page.keyboard.press('Escape')

  const outputModalities = page.locator('[data-modality-select="output"]')
  await expect(outputModalities).toContainText('text')
  await outputModalities.click()
  for (const value of ['text', 'image', 'audio', 'video', 'pdf']) {
    await expect(page.getByRole('option', { name: value, exact: true })).toBeVisible()
  }
  await page.getByRole('option', { name: 'image', exact: true }).click()
  await page.keyboard.press('Escape')
  const effortValuesSelect = page.locator('[data-effort-values-select]')
  await expect(effortValuesSelect).toContainText('low, medium, high, future')
  const scrollOwner = page.locator('main.shell-main')
  const expectedScrollTop = await effortValuesSelect.evaluate((element) => {
    const owner = document.querySelector<HTMLElement>('main.shell-main')
    if (!owner) throw new Error('Page scroll owner is missing')
    const control = element.getBoundingClientRect()
    const container = owner.getBoundingClientRect()
    owner.scrollTop += control.top - container.top - 120
    return owner.scrollTop
  })
  await expect.poll(() => scrollOwner.evaluate((element) => element.scrollTop)).toBe(expectedScrollTop)
  await effortValuesSelect.click()
  for (const value of [
    'none',
    'minimal',
    'low',
    'medium',
    'high',
    'xhigh',
    'max',
    'default',
    'Use service default',
    'future',
  ]) {
    await expect(page.getByRole('option', { name: value, exact: true })).toBeVisible()
  }
  await page.getByRole('option', { name: 'none', exact: true }).click()
  await page.keyboard.press('Escape')
  await expect.poll(() => scrollOwner.evaluate((element) => element.scrollTop)).toBe(expectedScrollTop)
  await expect(page.locator('[data-reasoning-option-add="effort"]')).toHaveCount(0)
  await expect(page.locator('[data-reasoning-option-add="toggle"]')).toBeVisible()
  await expect(page.locator('[data-reasoning-option-add="budget_tokens"]')).toBeVisible()
  await scrollOwner.evaluate((element) => {
    element.scrollTop = 320
  })
  await expect.poll(() => scrollOwner.evaluate((element) => element.scrollTop)).toBe(320)
  await page.locator('[data-reasoning-option-add="toggle"]').evaluate((element: HTMLButtonElement) => {
    element.click()
  })
  await expect.poll(() => scrollOwner.evaluate((element) => element.scrollTop)).toBe(320)
  await expect(page.locator('[data-reasoning-option-add="toggle"]')).toHaveCount(0)
  await expect(page.locator('[data-reasoning-option-add="effort"]')).toHaveCount(0)
  await page.locator('#provider-model-cost-input').fill('0.123456789012345678')
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
  expect(savedMetadata.cost).not.toHaveProperty('context_over_200k')
  expect(savedMetadata.cost).toMatchObject({ reasoning: 2, input_audio: 3, output_audio: 4 })
  expect(savedMetadata.cost.tiers).toEqual([
    expect.objectContaining({
      tier: { type: 'context', size: 272000 },
      input: 6,
      output: 24,
      reasoning: 7,
      input_audio: 8,
      output_audio: 9,
    }),
  ])
  expect(savedMetadata.reasoning_options.map((option: { type: string }) => option.type)).toEqual(['effort', 'toggle'])
  expect(savedMetadata.reasoning_options.find((option: { type: string }) => option.type === 'effort').values).toEqual([
    'none',
    'low',
    'medium',
    'high',
    'future',
  ])

  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await page.getByRole('button', { name: 'Add model', exact: true }).click()
  const manualModelPicker = page.locator('#manual-provider-model-search')
  await manualModelPicker.click()
  const manualModelSearch = page.getByPlaceholder('Search model')
  await expect(manualModelSearch).toHaveAttribute('aria-label', 'Search model')
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
  await expect(page.locator('#provider-model-id')).toHaveValue('gpt-5.4')
  await expect(page.locator('#provider-model-name')).toHaveValue('GPT-5.4')
  await page.getByText('Advanced model settings', { exact: false }).click()
  await expect(page.getByText('Extension fields (read only) · 1')).toBeVisible()

  await page.getByRole('button', { name: 'Close model editor' }).click()
  await page.getByRole('button', { name: 'Add model', exact: true }).click()
  await manualModelPicker.click()
  await manualModelSearch.fill('openai/gpt-5.3-codex-spark')
  await page.getByRole('option', { name: /GPT-5\.3 Codex Spark.*openai\/gpt-5\.3-codex-spark/ }).click()
  const [manualDialogBounds, manualPickerBounds] = await Promise.all([
    page.getByRole('dialog', { name: 'Add a model' }).boundingBox(),
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
  await expect(page.getByRole('dialog', { name: 'Add a model' })).toBeVisible()
  await expect(manualModelPicker).toHaveText(/Claude Opus 4\.6.*anthropic\/claude-opus-4\.6/)
})

test('OAuth Provider configuration opens authorization without manual callback fields on localhost', async ({
  page,
}) => {
  await page.goto('/providers')
  await page.getByRole('button', { name: /Connect (first )?service/ }).click()
  await page.getByRole('button', { name: /Codex.*OAuth account/ }).click()

  await expect(page.getByLabel('Connection name')).toHaveValue('Codex')
  await expect(page.getByLabel('Service identifier')).toHaveCount(0)
  await expect(page.getByLabel('API Key')).toHaveCount(0)

  const initRequest = page.waitForRequest(
    (request) => request.url().endsWith('/api/v1/oauth/sessions/init') && request.method() === 'POST',
  )
  const popupPromise = page.waitForEvent('popup')
  await page.getByRole('button', { name: 'Sign in with OAuth' }).click()
  const popup = await popupPromise
  await popup.close()

  expect((await initRequest).postDataJSON()).toMatchObject({ vendor: 'codex', use_proxy: false, callback_mode: 'auto' })
  await expect(page.getByText('Waiting for authorization…')).toBeVisible()
  await expect(page.getByLabel('Callback URL')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Reopen sign-in page' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Cancel sign-in' })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel sign-in' }).click()
  await expect(page.getByRole('button', { name: 'Sign in with OAuth' })).toBeVisible()
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
          vendor: 'codex',
          scheme: 'oauth_auth_code_pkce',
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
  await page.getByRole('button', { name: /Connect (first )?service/ }).click()
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
    auth_mode: 'apikey',
    preset_key: 'openai',
    channel: 'default',
    models_source: 'catalog',
    static_models: null,
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  const available = {
    id: 'openai/gpt-test',
    name: 'GPT Test',
    available: true,
    source_kind: 'discovered',
    selection_policy: 'auto',
    capabilities: { tool_call: true, reasoning: true, attachment: false, context: 128000 },
    revision: 1,
  }
  const unavailable = {
    ...available,
    id: 'retired-model',
    name: 'Retired Model',
    available: false,
    source_kind: 'manual',
  }
  const detail = {
    ...available,
    can_reimport: true,
    metadata: {
      id: available.id,
      name: available.name,
      description: 'Editable metadata',
      reasoning: true,
      tool_call: true,
      modalities: { input: ['text'], output: ['text'] },
      limit: { context: 128000 },
    },
    extensions: {},
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  const routeModel = {
    id: 'route-detail',
    name: 'client-model',
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
  await expect(providerBreadcrumb.getByRole('link', { name: 'Model services' })).toHaveAttribute('href', '/providers')
  await expect(providerBreadcrumb.getByText(provider.name, { exact: true })).toHaveAttribute('aria-current', 'page')
  const providerViews = page.getByRole('navigation', { name: 'Model service details' })
  await expect(providerViews).toHaveCSS('border-bottom-width', '0px')
  await providerViews.getByRole('link', { name: 'Connection settings' }).click()
  const connectionSection = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Connection settings', level: 2 }) })
  await expect
    .poll(async () => {
      const [sectionBox, formBox] = await Promise.all([
        connectionSection.boundingBox(),
        connectionSection.locator('form').boundingBox(),
      ])
      return sectionBox && formBox ? Math.round((formBox.width / sectionBox.width) * 100) : 0
    })
    .toBeGreaterThanOrEqual(95)
  await providerViews.getByRole('link', { name: 'Available models' }).click()
  await expect(page.getByRole('link', { name: 'Connection settings' })).toBeVisible()
  await expect(page.getByRole('link', { name: 'Available models' })).toHaveAttribute('aria-current', 'page')
  const modelTable = page.getByRole('table', { name: 'Models from this service' })
  const modelTableContainer = modelTable.locator('xpath=..')
  expect(await modelTableContainer.evaluate((element) => element.scrollWidth)).toBeLessThanOrEqual(
    await modelTableContainer.evaluate((element) => element.clientWidth),
  )
  await expect(page.getByRole('button', { name: 'Clear filters' })).toBeVisible()
  await expect(page.locator('#provider-model-search-desktop')).toBeVisible()
  await expect(modelTable.locator('input')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Show filter menu for Model availability' })).toBeVisible()
  await expect
    .poll(() => modelTable.getByRole('columnheader').first().evaluate((header) => Math.round(header.getBoundingClientRect().height)))
    .toBeLessThanOrEqual(56)
  await expect(page.getByRole('row').filter({ hasText: /GPT Test.*openai\/gpt-test/ })).toBeVisible()
  await expect(page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })).toHaveCount(0)
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveCount(0)
  await expect(page.getByText('Use synced status')).toBeHidden()
  await page
    .getByRole('row')
    .filter({ hasText: /GPT Test.*openai\/gpt-test/ })
    .getByRole('cell')
    .nth(1)
    .click()
  await expect(page).toHaveURL(/view=models&model=openai%2Fgpt-test/)
  await page.getByRole('button', { name: 'Model actions' }).click()
  await page.getByRole('menuitem', { name: 'Restore details from service…' }).click()
  await expect(
    page.getByText(
      'This Provider Catalog Entry is no longer available. Your saved model is unchanged. Refresh the catalog or update it manually.',
    ),
  ).toBeVisible()
  await expect(page.locator('#provider-model-id')).toHaveValue('openai/gpt-test')
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  let availabilityFilterDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityFilterDialog
    .locator('[data-slot="select-trigger"][aria-label="Model availability"]')
    .click()
  await page.getByRole('option', { name: 'All models', exact: true }).click()
  await expect(page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })).toHaveCount(0)
  await availabilityFilterDialog.getByRole('button', { name: 'Apply' }).click()
  const filterColumnStarts = await modelTable
    .getByRole('columnheader')
    .evaluateAll((columns) =>
      columns.slice(0, 4).map((column) => Math.round(column.getBoundingClientRect().left)),
  )
  const modelRowColumns = await Promise.all(
    [
      page.getByRole('row').filter({ hasText: /GPT Test.*openai\/gpt-test/ }),
      page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ }),
    ].map((row) =>
      row
        .locator(':scope > td')
        .evaluateAll((columns) =>
          columns.slice(0, 4).map((column) => Math.round(column.getBoundingClientRect().left)),
        ),
    ),
  )
  expect(modelRowColumns).toEqual([filterColumnStarts, filterColumnStarts])
  await expect(page.getByRole('link', { name: 'Used by models' })).toBeVisible()
  await page.getByRole('button', { name: 'Sync models' }).click()
  await expect(page.getByText("Connection saved, but models couldn't be synced")).toBeVisible()
  await expect(page.getByText('Sync this list to check for model updates.')).toBeVisible()
  await expect(
    page.getByText(
      'Provider Catalog model details are temporarily unavailable. Your saved models are unchanged. Try syncing or re-importing again.',
    ),
  ).toBeVisible()

  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  availabilityFilterDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityFilterDialog
    .locator('[data-slot="select-trigger"][aria-label="Model availability"]')
    .click()
  await page.getByRole('option', { name: 'Unavailable', exact: true }).click()
  await availabilityFilterDialog.getByRole('button', { name: 'Apply' }).click()
  await page.locator('#provider-model-search-desktop').fill('retired')
  const retiredModelRow = page.getByRole('row').filter({ hasText: /Retired Model.*retired-model/ })
  await expect(retiredModelRow).toBeVisible()
  await expect(page.getByRole('link', { name: 'Used by 1 model', exact: true })).toBeVisible()

  await retiredModelRow.getByRole('cell').nth(2).click()
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
    .nth(1)
    .click()
  await page.getByRole('button', { name: 'Model actions' }).click()
  await page.getByRole('menuitem', { name: 'Remove manually added model…' }).click()
  await expect(page.getByRole('alertdialog', { name: 'Delete Retired Model?' })).toBeVisible()
  await expect(page.getByText(/1 configured model will continue using the same service model ID/)).toBeVisible()
  await expect(page.getByRole('link', { name: /client-model.*retired-model/ })).toBeVisible()
  await page.getByRole('alertdialog', { name: 'Delete Retired Model?' }).getByRole('button', { name: 'Cancel' }).click()
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()

  await page.getByRole('link', { name: 'Used by models' }).click()
  await expect(page.getByRole('link', { name: 'client-model' })).toBeVisible()
  await expect(page.getByText('retired-model', { exact: true })).toBeVisible()
  routeQueryFails = true
  await page.goto(`/providers/${provider.id}?view=models`)
  await page.getByRole('button', { name: 'Show filter menu for Model availability' }).click()
  availabilityFilterDialog = page.getByRole('dialog', { name: 'Filter by Model availability' })
  await availabilityFilterDialog
    .locator('[data-slot="select-trigger"][aria-label="Model availability"]')
    .click()
  await page.getByRole('option', { name: 'Unavailable', exact: true }).click()
  await availabilityFilterDialog.getByRole('button', { name: 'Apply' }).click()
  await page
    .getByRole('row')
    .filter({ hasText: /Retired Model.*retired-model/ })
    .getByRole('cell')
    .nth(2)
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
    name: 'used-route',
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

  await page.getByRole('menuitem', { name: 'Delete service…' }).click()
  await expect(page.getByRole('alertdialog', { name: `Cannot delete ${provider.name}` })).toBeVisible()
  await expect(page.getByRole('link', { name: /used-route.*upstream/ })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Delete service' })).toBeDisabled()
  await expect(providerDeletes).toBe(0)
  await page.getByRole('button', { name: 'Cancel' }).click()
  await page.getByRole('button', { name: `More actions for ${safeProvider.name}` }).click()
  await page.getByRole('menuitem', { name: 'Delete service…' }).click()
  await expect(page.getByRole('alertdialog', { name: `Delete ${safeProvider.name}?` })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Delete service' })).toBeEnabled()
  await page.getByRole('button', { name: 'Delete service' }).click()
  await expect.poll(() => providerDeletes).toBe(1)

  await page.goto('/models')
  await page.getByRole('button', { name: `More actions for ${routeModel.name}` }).click()
  await page.getByRole('menuitem', { name: 'Delete model…' }).click()
  await expect(page.getByRole('alertdialog', { name: `Delete ${routeModel.name}?` })).toBeVisible()
  await expect(page.getByText('1 API Key permission will be removed')).toBeVisible()
  await expect(page.getByText('Production key', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Cancel' }).click()
})

test('creating a Provider continues into detail and reports automatic model sync', async ({ page }) => {
  const createdProvider = {
    id: 'created-provider',
    name: 'Created Provider',
    vendor: 'custom',
    protocol: 'openai-compatible',
    base_url: 'https://created.example/v1',
    use_proxy: false,
    auth_mode: 'apikey',
    preset_key: null,
    channel: null,
    models_source: null,
    static_models: null,
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    updated_at: '2026-08-17T00:00:00Z',
  }
  let created = false
  let syncCalls = 0
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers' && request.method() === 'GET') {
      await route.fulfill({ json: { data: created ? [createdProvider] : [] } })
      return
    }
    if (path === '/providers' && request.method() === 'POST') {
      created = true
      await route.fulfill({ json: { data: createdProvider } })
      return
    }
    if (path === `/providers/${createdProvider.id}/models/sync` && request.method() === 'POST') {
      syncCalls += 1
      const { promise, resolve } = Promise.withResolvers<void>()
      setTimeout(resolve, 150)
      await promise
      await route.fulfill({ json: { data: { added: 3, missing: 1, restored: 2, deprecated: 0 } } })
      return
    }
    if (path === `/providers/${createdProvider.id}/models` && request.method() === 'GET') {
      await route.fulfill({ json: { data: { models: [] } } })
      return
    }
    await route.fallback()
  })

  await page.goto('/providers')
  await page.getByRole('button', { name: /Connect (first )?service/ }).click()
  await page.getByRole('button', { name: /Custom.*Bring your own/ }).click()
  await page.getByLabel('Connection name').fill(createdProvider.name)
  await page.getByLabel('Base URL').fill(createdProvider.base_url)
  await page.getByRole('button', { name: 'Connect', exact: true }).click()

  await expect(page).toHaveURL(new RegExp(`/providers/${createdProvider.id}\\?view=models`))
  await expect(page.getByText('Syncing models…')).toBeVisible()
  await expect(page.getByText('3 new · 1 no longer offered · 2 available again', { exact: true })).toBeVisible()
  const inventorySummary = page.getByRole('heading', { name: 'Models from this service' }).locator('..')
  const freshness = inventorySummary.locator('p').filter({ hasText: 'Last checked' })
  await expect(freshness).toBeVisible()
  await expect(freshness).toContainText('3 new')
  await expect(freshness).toContainText('1 no longer offered')
  await expect(freshness).toContainText('2 available again')
  await expect.poll(() => syncCalls).toBe(1)
  await expect(page.getByRole('link', { name: 'Use a model' })).toHaveAttribute(
    'href',
    `/models/new?provider=${createdProvider.id}`,
  )
})

test('unused provider models create a matching model or append a destination', async ({ page }) => {
  const provider = {
    id: 'provider-primary',
    name: 'Primary Provider',
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
      selection_policy: 'auto',
      capabilities: { attachment: false, reasoning: true, tool_call: true, context: 128000 },
      revision: 1,
    },
    {
      id: 'gpt-new',
      name: 'GPT New',
      available: true,
      source_kind: 'discovered',
      selection_policy: 'auto',
      capabilities: { attachment: false, reasoning: true, tool_call: true, context: 128000 },
      revision: 1,
    },
  ]
  let routes = [
    {
      id: 'route-existing',
      name: 'gpt-existing',
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
        ]
        await route.fulfill({ json: { data: routes[0] } })
        return
      }
      const created = {
        id: 'route-new',
        name: 'gpt-new',
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
  await expect(existingAction).toBeVisible()
  await existingAction.hover()
  await expect(existingAction.getByText('Add destination', { exact: true })).toBeVisible()

  await existingAction.click()
  await expect(page.getByText('Added this service to model gpt-existing.')).toBeVisible()
  expect(bindBodies[0]).toEqual({ provider_id: provider.id, provider_model_id: 'gpt-existing' })

  const newAction = page.getByRole('button', { name: 'Add gpt-new to a model' })
  await newAction.hover()
  await expect(newAction.getByText('Create model', { exact: true })).toBeVisible()
  await newAction.click()
  await expect(page.getByText('Created model gpt-new.')).toBeVisible()
  expect(bindBodies[1]).toEqual({ provider_id: provider.id, provider_model_id: 'gpt-new' })
})
