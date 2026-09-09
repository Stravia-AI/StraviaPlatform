import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

const thinkingLevels = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const

test('prefilled model metadata stays clean without erasing a user draft', async ({ page }) => {
  let holdDetails = false
  let releaseDetails!: () => void
  const detailsReady = new Promise<void>((resolve) => {
    releaseDetails = resolve
  })
  await page.route('**/api/v1/providers', (route) =>
    route.fulfill({
      json: {
        data: [{ id: 'prefill-provider', name: 'Prefill service', protocol: 'openai-compatible', is_enabled: true }],
      },
    }),
  )
  await page.route('**/api/v1/providers/prefill-provider/models', (route) =>
    route.fulfill({
      json: {
        data: {
          models: [
            {
              id: 'prefill-model',
              name: 'Prefill model',
              available: true,
              source_kind: 'discovered',
              selection_policy: 'auto',
              specification: {
                limit: null,
                modalities: null,
                reasoning: null,
                tool_call: null,
                structured_output: null,
                attachment: null,
                temperature: null,
              },
              revision: 1,
            },
          ],
        },
      },
    }),
  )
  await page.route('**/api/v1/providers/prefill-provider/model-capabilities?*', (route) =>
    route.fulfill({ json: { data: { reasoning: true } } }),
  )
  await page.route('**/api/v1/providers/prefill-provider/model?*', async (route) => {
    if (holdDetails) await detailsReady
    await route.fulfill({
      json: {
        data: {
          id: 'prefill-model',
          metadata: {},
          thinking_level_map: [{ level: 'high', control: { type: 'effort', value: 'high' }, source: 'generated' }],
        },
      },
    })
  })

  await page.goto('/models/new?provider=prefill-provider&model=prefill-model')
  await expect(page.locator('[data-slot="route-thinking-level"][data-level="high"]')).toHaveAttribute(
    'data-supported',
    'true',
  )
  await page.getByRole('link', { name: 'Cancel', exact: true }).click()
  await expect(page).toHaveURL(/\/models$/)
  await expect(page.getByRole('alertdialog')).toHaveCount(0)

  holdDetails = true
  await page.goto('/models/new?provider=prefill-provider&model=prefill-model')
  await page.getByLabel('Display name', { exact: true }).fill('My unsaved label')
  releaseDetails()
  await expect(page.locator('[data-slot="route-thinking-level"][data-level="high"]')).toHaveAttribute(
    'data-supported',
    'true',
  )
  await page.getByRole('link', { name: 'Cancel', exact: true }).click()
  await page.getByRole('alertdialog').getByRole('button', { name: 'Keep editing' }).click()
  await expect(page.getByLabel('Display name', { exact: true })).toHaveValue('My unsaved label')
})

function thinkingMap(visible: ReadonlySet<string>) {
  return thinkingLevels.map((level) => ({
    level,
    control: visible.has(level) ? { type: 'effort', value: level === 'off' ? 'none' : level } : { type: 'hidden' },
    source: 'generated',
  }))
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('Models table fits the desktop content width without horizontal scrolling', async ({ page }) => {
  await page.setViewportSize({ width: 1279, height: 800 })
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'codex',
            name: 'Codex',
            protocol: 'open-responses',
            base_url: 'https://codex.example/v1',
            is_enabled: true,
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({
      json: {
        data: ['grok-4.6', 'gpt-5.6-sol', 'gpt-5.6-luna'].map((modelId) => ({
          id: modelId,
          model_id: modelId,
          display_name: modelId === 'gpt-5.6-sol' ? 'GPT 5.6 Sol' : null,
          balance: 'weighted',
          is_enabled: true,
          targets: [
            {
              id: `${modelId}-target`,
              model_id: modelId,
              provider_id: 'codex',
              model: modelId,
              enabled: true,
              priority: 1,
            },
            ...(modelId === 'gpt-5.6-sol'
              ? [
                  {
                    id: `${modelId}-disabled-target`,
                    model_id: modelId,
                    provider_id: 'codex',
                    model: 'disabled-shadow-model',
                    enabled: false,
                    priority: 0,
                  },
                ]
              : []),
          ],
        })),
      },
    })
  })
  await page.goto('/models')
  const tableContainer = page.locator('[data-slot="table-container"]')
  await expect(tableContainer).toBeVisible()
  await expect(tableContainer.getByRole('columnheader', { name: 'Display name' })).toBeVisible()
  await expect(tableContainer.getByRole('columnheader', { name: 'Client Model ID' })).toBeVisible()
  await expect(tableContainer.getByRole('columnheader', { name: 'Associated services' })).toBeVisible()
  await expect(tableContainer.getByText('GPT 5.6 Sol', { exact: true })).toBeVisible()
  await expect(tableContainer.getByText('gpt-5.6-sol', { exact: true })).toBeVisible()
  await expect(tableContainer.getByText('grok-4.6', { exact: true })).toHaveCount(2)
  await expect(tableContainer.getByText('disabled-shadow-model', { exact: true })).toHaveCount(0)
  const rows = tableContainer.getByRole('row')
  await expect(rows.nth(1)).toContainText('GPT 5.6 Sol')
  await expect(rows.nth(1)).toContainText('gpt-5.6-sol')
  await expect(rows.nth(1)).toContainText('Codex')
  await expect(rows.nth(1)).toContainText('Enabled')
  await expect(rows.nth(1)).not.toContainText('disabled-shadow-model')
  await expect(rows.nth(2)).toContainText('gpt-5.6-luna')
  await expect(rows.nth(3)).toContainText('grok-4.6')
  expect(await tableContainer.evaluate((element) => element.scrollWidth)).toBeLessThanOrEqual(
    await tableContainer.evaluate((element) => element.clientWidth),
  )
})

test('Mobile model links preserve new-tab navigation and keep row actions independent', async ({ page, context }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route('**/api/v1/providers', (route) =>
    route.fulfill({
      json: {
        data: [{ id: 'mobile-provider', name: 'Mobile provider', protocol: 'open-responses', is_enabled: true }],
      },
    }),
  )
  await page.route('**/api/v1/models', (route) =>
    route.fulfill({
      json: {
        data: [
          {
            id: 'mobile-route',
            model_id: 'mobile-model',
            display_name: 'Mobile model',
            balance: 'traffic_equalization',
            is_enabled: true,
            targets: [],
          },
        ],
      },
    }),
  )
  await page.goto('/models')
  const modelLink = page.getByRole('link', { name: /^Mobile model/ })
  await expect(modelLink).toBeVisible()
  await page.getByRole('button', { name: 'More actions for Mobile model', exact: true }).click()
  await expect(page.getByRole('menuitem', { name: 'Disable model', exact: true })).toBeVisible()
  await expect(page).toHaveURL(/\/models$/)
  await page.keyboard.press('Escape')

  await context.route('**/api/v1/auth/state', (route) =>
    route.fulfill({
      json: { mode: 'server', authenticated: true, setup_authorized: false, username: 'playwright-admin' },
    }),
  )
  const newTab = context.waitForEvent('page')
  await modelLink.click({ modifiers: ['ControlOrMeta'] })
  const opened = await newTab
  await prepareApp(opened)
  await expect(opened).toHaveURL(/\/models\/mobile-model$/)
  await expect(page).toHaveURL(/\/models$/)
  await opened.close()

  await modelLink.focus()
  await page.keyboard.press('Enter')
  await expect(page).toHaveURL(/\/models\/mobile-model$/)
})

test('Model Route curl always includes the selected API Key', async ({ page }) => {
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'model-gpt',
            model_id: 'gpt-5.4',
            display_name: 'GPT 5.4',
            balance: 'priority',
            target_provider: 'provider',
            target_model: 'gpt-5.4',
            is_enabled: true,
            created_at: '2026-08-05T00:00:00Z',
            targets: [],
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/api-keys', async (route) => {
    await route.fulfill({
      json: {
        data: [{ id: 'key-gpt', key: 'sk-test-secret', name: 'GPT key', is_enabled: true, model_ids: ['model-gpt'] }],
      },
    })
  })

  await page.goto('/connect')
  await page.getByRole('tab', { name: 'Code' }).click()
  await page.getByRole('button', { name: 'Model' }).click()
  await page.getByRole('option', { name: 'gpt-5.4' }).click()
  await page.getByRole('tab', { name: 'cURL' }).click()

  const generatedRequest = page.locator('pre.route-code-plane')
  await expect(generatedRequest).toContainText('Authorization')
  await expect(generatedRequest).toContainText('sk-test-secret')
  await expect(generatedRequest).toContainText('gpt-5.4')
  await expect(page.getByRole('button', { name: 'Copy', exact: true })).toBeEnabled()
})

test('Model Route editor omits API Key and payload toggles', async ({ page }) => {
  await page.setViewportSize({ width: 1089, height: 612 })
  const model = {
    id: 'model-gpt',
    model_id: 'gpt-5.4',
    display_name: 'Team GPT',
    balance: 'priority',
    target_provider: 'provider',
    target_model: 'gpt-5.4',
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    targets: [
      {
        id: 'target-gpt',
        model_id: 'model-gpt',
        provider_id: 'provider',
        model: 'gpt-5.4',
        weight: 100,
        priority: 1,
        created_at: '2026-08-17T00:00:00Z',
      },
    ],
  }
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'provider',
            name: 'Provider',
            protocol: 'openai-compatible',
            base_url: 'https://provider.example/v1',
            use_proxy: false,
            is_enabled: true,
            created_at: '2026-08-17T00:00:00Z',
            updated_at: '2026-08-17T00:00:00Z',
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [model] } })
  })
  await page.route('**/api/v1/models/gpt-5.4', async (route) => {
    await route.fulfill({ json: { data: model } })
  })

  await page.goto('/models')
  await expect(page.getByRole('link', { name: 'Edit' })).toHaveCount(0)
  await page
    .locator('main')
    .getByRole('link')
    .filter({ hasText: 'gpt-5.4' })
    .getByText('gpt-5.4', { exact: true })
    .click()
  await expect(page.getByRole('heading', { name: 'Edit model' })).toBeVisible()
  const editModelId = page.getByRole('combobox', { name: 'Model ID', exact: true })
  const editDisplayName = page.getByLabel('Display name', { exact: true })
  await expect(editModelId).toHaveValue('gpt-5.4')
  await expect(editDisplayName).toHaveValue('Team GPT')
  await editModelId.fill('custom/edit-model')
  await editModelId.press('Enter')
  await expect(editModelId).toHaveValue('custom/edit-model')
  await expect(editDisplayName).toHaveValue('Team GPT')
  const editRouteBreadcrumb = page.getByRole('navigation', { name: 'Breadcrumb' })
  await expect(editRouteBreadcrumb.getByRole('link', { name: 'Models' })).toHaveAttribute('href', '/models')
  await expect(editRouteBreadcrumb.getByText('Edit', { exact: true })).toHaveAttribute('aria-current', 'page')
  await expect(page.getByText('Require API key', { exact: true })).toHaveCount(0)
  await expect(page.getByText('Save request and response content', { exact: true })).toHaveCount(0)

  const scrollPane = page.locator('.shell-scrollbar')
  const footer = page.locator('[data-slot="model-editor-footer"]')
  const initialFooter = await footer.boundingBox()
  await scrollPane.evaluate((element) => element.scrollTo({ top: element.scrollHeight }))
  await expect.poll(async () => scrollPane.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
  const scrolledFooter = await footer.boundingBox()
  expect(Math.abs((scrolledFooter?.y ?? 0) - (initialFooter?.y ?? 0))).toBeLessThanOrEqual(1)
  const footerCoverage = await footer.evaluate((element) => {
    const footerBox = element.getBoundingClientRect()
    const scrollBox = element.closest('.shell-scrollbar')?.getBoundingClientRect()
    const extension = Number.parseFloat(getComputedStyle(element, '::after').height) || 0
    return { coveredBottom: footerBox.bottom + extension, scrollBottom: scrollBox?.bottom ?? 0 }
  })
  expect(Math.abs(footerCoverage.coveredBottom - footerCoverage.scrollBottom)).toBeLessThanOrEqual(1)
  const saveButtonBottom = await footer.evaluate((element) => {
    const buttons = element.querySelectorAll<HTMLElement>('[data-slot="button"]')
    return buttons.item(buttons.length - 1).getBoundingClientRect().bottom
  })
  expect(footerCoverage.scrollBottom - saveButtonBottom).toBeLessThanOrEqual(20)

  await page.setViewportSize({ width: 1089, height: 1600 })
  await scrollPane.evaluate((element) => element.scrollTo({ top: 0 }))
  const shortContentCoverage = await footer.evaluate((element) => {
    const footerBox = element.getBoundingClientRect()
    const scrollBox = element.closest('.shell-scrollbar')?.getBoundingClientRect()
    const extension = Number.parseFloat(getComputedStyle(element, '::after').height) || 0
    return {
      coveredBottom: footerBox.bottom + extension,
      scrollBottom: scrollBox?.bottom ?? 0,
      scrollHeight: element.closest('.shell-scrollbar')?.scrollHeight ?? 0,
      clientHeight: element.closest('.shell-scrollbar')?.clientHeight ?? 0,
    }
  })
  expect(shortContentCoverage.scrollHeight).toBe(shortContentCoverage.clientHeight)
  expect(Math.abs(shortContentCoverage.coveredBottom - shortContentCoverage.scrollBottom)).toBeLessThanOrEqual(1)
})

test('Model Route editor derives thinking levels and identifies blocking destinations', async ({ page }) => {
  await page.setViewportSize({ width: 820, height: 900 })
  let updateBody: Record<string, unknown> | undefined
  const model = {
    id: 'thinking-route',
    model_id: 'thinking-route',
    display_name: null,
    balance: 'priority',
    target_provider: 'provider',
    target_model: 'wide-model',
    is_enabled: true,
    created_at: '2026-08-17T00:00:00Z',
    supported_thinking_levels: ['off', 'max'],
    targets: [
      {
        id: 'target-wide',
        model_id: 'thinking-route',
        provider_id: 'provider',
        model: 'wide-model',
        enabled: true,
        priority: 1,
        first_token_timeout_ms: 60_000,
        target_retry_budget: 5,
        target_cooldown_ms: 120_000,
        created_at: '2026-08-17T00:00:00Z',
        thinking_level_map: thinkingMap(new Set(['off', 'low', 'high'])),
      },
      {
        id: 'target-narrow',
        model_id: 'thinking-route',
        provider_id: 'provider',
        model: 'narrow-model',
        enabled: true,
        priority: 2,
        first_token_timeout_ms: 60_000,
        target_retry_budget: 5,
        target_cooldown_ms: 120_000,
        created_at: '2026-08-17T00:00:00Z',
        thinking_level_map: thinkingMap(new Set(['low', 'high'])),
      },
    ],
  }
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({
      json: {
        data: [
          {
            id: 'provider',
            name: 'Provider',
            protocol: 'openai-compatible',
            base_url: 'https://provider.example/v1',
            use_proxy: false,
            is_enabled: true,
            created_at: '2026-08-17T00:00:00Z',
            updated_at: '2026-08-17T00:00:00Z',
          },
        ],
      },
    })
  })
  await page.route('**/api/v1/models', async (route) => {
    await route.fulfill({ json: { data: [model] } })
  })
  await page.route('**/api/v1/models/thinking-route', async (route) => {
    if (route.request().method() === 'GET') {
      await route.fulfill({ json: { data: model } })
      return
    }
    updateBody = route.request().postDataJSON() as Record<string, unknown>
    await route.fulfill({
      json: {
        data: {
          id: 'thinking-route',
          model_id: 'thinking-route',
          display_name: null,
          balance: 'priority',
          target_provider: 'provider',
          target_model: 'wide-model',
          is_enabled: updateBody.is_enabled,
          created_at: '2026-08-17T00:00:00Z',
          supported_thinking_levels: ['low', 'high'],
          targets: [],
        },
      },
    })
  })

  await page.goto('/models')
  await page.locator('main').getByRole('link').filter({ hasText: 'thinking-route' }).click()
  await expect(page.getByRole('button', { name: 'Advanced', exact: true })).toHaveCount(0)
  const enabledSwitch = page.getByRole('switch', { name: 'Enabled', exact: true })
  await expect(enabledSwitch).toBeChecked()
  const enabledControl = page.locator('[data-slot="model-enabled-control"]')
  const [switchBox, controlBox] = await Promise.all([enabledSwitch.boundingBox(), enabledControl.boundingBox()])
  expect((switchBox?.x ?? 0) + (switchBox?.width ?? 0) <= (controlBox?.x ?? 0) + (controlBox?.width ?? 0)).toBe(true)
  await expect
    .poll(() => page.locator('.shell-scrollbar').evaluate((element) => element.scrollWidth - element.clientWidth))
    .toBe(0)
  await enabledSwitch.click()
  await expect(enabledSwitch).not.toBeChecked()

  await page.setViewportSize({ width: 1568, height: 900 })
  const [nameLabelBox, nameInputBox, displayLabelBox, displayInputBox, balanceLabelBox, balanceTriggerBox] =
    await Promise.all([
      page.getByText('Model ID', { exact: true }).boundingBox(),
      page.locator('#route-model-id').boundingBox(),
      page.getByText('Display name', { exact: true }).boundingBox(),
      page.locator('#route-display-name').boundingBox(),
      page.getByText('How requests are sent', { exact: true }).boundingBox(),
      page.locator('#route-balance').boundingBox(),
    ])
  expect(nameLabelBox).not.toBeNull()
  expect(nameInputBox).not.toBeNull()
  expect(displayLabelBox).not.toBeNull()
  expect(displayInputBox).not.toBeNull()
  expect(balanceLabelBox).not.toBeNull()
  expect(balanceTriggerBox).not.toBeNull()
  expect(Math.abs(nameLabelBox!.x - nameInputBox!.x)).toBeLessThanOrEqual(1)
  expect(Math.abs(displayLabelBox!.x - displayInputBox!.x)).toBeLessThanOrEqual(1)
  expect(Math.abs(balanceLabelBox!.x - balanceTriggerBox!.x)).toBeLessThanOrEqual(1)
  expect(Math.abs(displayInputBox!.width - balanceTriggerBox!.width)).toBeLessThanOrEqual(2)

  const thinkingMaps = page.locator('[data-slot="thinking-map"]')
  for (const index of [1, 2]) {
    await page.getByRole('button', { name: `Edit destination ${index}` }).click()
    await expect(thinkingMaps).toHaveCount(1)
    await expect(page.locator('[data-slot="thinking-map-row"]')).toHaveCount(7)
    await expect(page.getByText('Generated', { exact: true })).toHaveCount(0)
    await page.getByRole('button', { name: 'Confirm' }).click()
  }

  const levelCards = page.locator('[data-slot="route-thinking-level"]')
  await expect(levelCards).toHaveCount(7)
  await expect(levelCards).toHaveText(thinkingLevels)
  await expect(page.locator('[data-slot="route-thinking-level"][data-level="low"]')).toHaveAttribute(
    'data-supported',
    'true',
  )
  await expect(page.locator('[data-slot="route-thinking-level"][data-level="high"]')).toHaveAttribute(
    'data-supported',
    'true',
  )
  const maxLevel = page.locator('[data-slot="route-thinking-level"][data-level="max"]')
  await expect(maxLevel).toHaveAttribute('data-supported', 'false')
  await maxLevel.hover()
  const blockedTooltip = page.locator('[data-slot="tooltip-content"]')
  await expect(blockedTooltip).toContainText('Blocked by these destinations:')
  await expect(blockedTooltip).toContainText('Destination 1 · Provider · wide-model')
  await expect(blockedTooltip).toContainText('Destination 2 · Provider · narrow-model')
  await expect(page.locator('[id^="thinking-level-"]')).toHaveCount(0)
  await page.getByRole('button', { name: 'Save model' }).click()
  await expect.poll(() => updateBody?.is_enabled).toBe(false)
})

test('Route Builder loads Provider Models and edits priority-lane destinations in dialogs', async ({ page }) => {
  const providers = [
    {
      id: 'provider-a',
      name: 'Provider A',
      protocol: 'openai-compatible',
      base_url: 'https://a.example/v1',
      use_proxy: false,
      is_enabled: true,
      created_at: '2026-08-17T00:00:00Z',
      updated_at: '2026-08-17T00:00:00Z',
    },
    {
      id: 'provider-b',
      name: 'Provider B',
      protocol: 'openai-compatible',
      base_url: 'https://b.example/v1',
      use_proxy: false,
      is_enabled: true,
      created_at: '2026-08-17T00:00:00Z',
      updated_at: '2026-08-17T00:00:00Z',
    },
  ]
  const available = {
    id: 'gpt-available',
    name: 'GPT Available',
    available: true,
    source_kind: 'discovered',
    selection_policy: 'auto',
    specification: {
      limit: { context: 1050000, input: 1048576, output: 32000 },
      modalities: { input: ['image', 'pdf'], output: ['text'] },
      reasoning: true,
      tool_call: false,
      structured_output: null,
      attachment: true,
      temperature: false,
    },
    revision: 1,
  }
  const unavailable = { ...available, id: 'gpt-unavailable', name: 'GPT Unavailable', available: false }
  const imageModel = { ...available, id: 'image-model', name: 'Image Model' }
  let createAttempts = 0
  let createBody: Record<string, unknown> | undefined
  let disableBody: Record<string, unknown> | undefined

  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const path = new URL(request.url()).pathname.replace('/api/v1', '')
    if (path === '/providers') {
      await route.fulfill({ json: { data: providers } })
      return
    }
    if (path === '/providers/provider-image/models') {
      await route.fulfill({ json: { data: { models: [imageModel] } } })
      return
    }
    if (path.endsWith('/models')) {
      await route.fulfill({ json: { data: { models: [available, unavailable] } } })
      return
    }
    if (path === '/providers/provider-a/model' && request.method() === 'GET') {
      await route.fulfill({
        json: {
          data: {
            ...available,
            metadata: {
              id: available.id,
              name: available.name,
              limit: available.specification.limit,
              modalities: available.specification.modalities,
              reasoning: true,
              tool_call: false,
              structured_output: null,
              attachment: true,
              temperature: false,
              cost: { input: 0.25, output: 1 },
            },
            extensions: {},
            created_at: '2026-08-17T00:00:00Z',
            updated_at: '2026-08-17T00:00:00Z',
          },
        },
      })
      return
    }
    if (path.endsWith('/model-capabilities')) {
      await route.fulfill({
        json: {
          data: {
            provider: 'Provider A',
            model_id: available.id,
            context_window: 128000,
            tool_call: true,
            reasoning: true,
            input_modalities: ['text', 'image'],
            output_modalities: ['text'],
          },
        },
      })
      return
    }
    await route.fallback()
  })
  await page.route('**/api/v1/models/route-created', async (route) => {
    disableBody = route.request().postDataJSON()
    await route.fulfill({ json: { data: { id: 'route-created' } } })
  })
  await page.route('**/api/v1/models', async (route) => {
    if (route.request().method() === 'POST') {
      createAttempts += 1
      createBody = route.request().postDataJSON() as Record<string, unknown>
      if (createAttempts === 1) {
        await route.fulfill({ status: 400, json: { error: 'Route validation failed' } })
      } else {
        await route.fulfill({ json: { data: { id: 'route-created', model_id: 'route-created' } } })
      }
      return
    }
    await route.fulfill({ json: { data: [] } })
  })

  await page.goto('/models/new')
  await page.getByRole('link', { name: 'Cancel' }).click()
  await expect(page).toHaveURL(/\/models$/)
  await expect(page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })).toHaveCount(0)

  await page.goto('/models/new')
  const modelSearch = page.getByRole('combobox', { name: 'Model ID', exact: true })
  const displayName = page.getByLabel('Display name', { exact: true })
  await modelSearch.fill('draft-model')
  const beforeUnloadDialog = page.waitForEvent('dialog')
  const reloadAttempt = page.evaluate(() => window.location.reload())
  const browserLeaveWarning = await beforeUnloadDialog
  expect(browserLeaveWarning.type()).toBe('beforeunload')
  await browserLeaveWarning.dismiss()
  await reloadAttempt
  await expect(modelSearch).toHaveValue('draft-model')

  await page.getByRole('link', { name: 'Cancel' }).click()
  const discardDialog = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })
  await expect(discardDialog).toBeVisible()
  await discardDialog.getByRole('button', { name: 'Keep editing' }).click()
  await expect(page).toHaveURL(/\/models\/new$/)
  await expect(modelSearch).toHaveValue('draft-model')
  await page.getByRole('link', { name: 'Cancel' }).click()
  await discardDialog.getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/\/models$/)

  await page.goto('/models/new')
  await expect(page.getByRole('heading', { name: 'Add model' })).toBeVisible()
  const createRouteBreadcrumb = page.getByRole('navigation', { name: 'Breadcrumb' })
  await expect(createRouteBreadcrumb.getByRole('link', { name: 'Models' })).toHaveAttribute('href', '/models')
  await expect(createRouteBreadcrumb.getByText('Create', { exact: true })).toHaveAttribute('aria-current', 'page')
  await expect(modelSearch).toHaveAttribute('placeholder', 'Search or enter Model ID')
  await expect(displayName).toHaveAttribute('placeholder', 'e.g. GPT-5.4')
  await modelSearch.fill('GPT-5.4')
  await expect(page.getByRole('option', { name: /GPT-5\.4.*openai\/gpt-5\.4/ })).toBeVisible()
  await page.getByRole('option', { name: /GPT-5\.4.*openai\/gpt-5\.4/ }).click()
  await expect(modelSearch).toHaveValue('gpt-5.4')
  await expect(displayName).toHaveValue('GPT-5.4')
  await modelSearch.fill('Claude Opus')
  await page.getByRole('option', { name: /Claude Opus 4\.6.*anthropic\/claude-opus-4\.6/ }).click()
  await expect(modelSearch).toHaveValue('claude-opus-4.6')
  await expect(displayName).toHaveValue('Claude Opus 4.6')
  await displayName.fill('Team GPT')
  await modelSearch.fill('custom/model')
  await modelSearch.press('Enter')
  await expect(modelSearch).toHaveValue('custom/model')
  await expect(displayName).toHaveValue('Team GPT')
  expect(createAttempts).toBe(0)
  await page.getByRole('button', { name: 'Clear selected model' }).click()
  await expect(modelSearch).toHaveValue('')
  await expect(displayName).toHaveValue('')
  await modelSearch.fill('openai/gpt-5.4')
  await page.getByRole('option', { name: /GPT-5\.4.*openai\/gpt-5\.4/ }).click()
  await displayName.fill('')
  await page.getByRole('button', { name: 'Add destination' }).click()
  await page.getByLabel('Destination 1 model service', { exact: true }).click()
  await page.getByRole('option', { name: 'Provider A' }).click()
  await expect(page.getByLabel('Destination 1 model', { exact: true })).toBeEnabled()
  await page.getByLabel('Destination 1 model', { exact: true }).click()
  await expect(page.getByRole('option', { name: /GPT Available.*gpt-available/ })).toBeVisible()
  await expect(page.getByRole('option', { name: /GPT Unavailable/ })).toHaveCount(0)
  await page.getByRole('option', { name: /GPT Available.*gpt-available/ }).click()
  const dialogSpecification = page.getByRole('dialog').getByRole('group', { name: 'Model specification' })
  await expect(dialogSpecification).toContainText('1.05M')
  await expect(dialogSpecification).toContainText('32K')
  await expect(dialogSpecification).toContainText('Input')
  await expect(dialogSpecification).toContainText('Output')
  await expect(dialogSpecification.getByRole('button', { name: 'Image input' })).toBeVisible()
  await expect(dialogSpecification.getByRole('button', { name: 'PDF input' })).toBeVisible()
  await expect(dialogSpecification.getByRole('button', { name: 'Text output' })).toBeVisible()
  await expect(dialogSpecification.getByRole('button', { name: 'Reasoning' })).toBeVisible()
  await page.getByRole('button', { name: 'View model details' }).click()
  const detailDialog = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'GPT Available' }) })
  const detailSpecification = detailDialog.getByRole('region', { name: 'Model specification' })
  await expect(detailSpecification).toContainText('1,050,000 tokens')
  await expect(detailSpecification).toContainText('1,048,576 tokens')
  await expect(detailSpecification).toContainText('32,000 tokens')
  await expect(detailSpecification).toContainText(/Input modalities.*Image.*PDF/)
  await expect(detailSpecification).toContainText(/Output modalities.*Text/)
  await expect(detailSpecification).toContainText(/Reasoning.*Supported/)
  await expect(detailSpecification).toContainText(/Tool calls.*Not supported/)
  await expect(detailSpecification).toContainText(/Structured output.*Not registered/)
  await expect(detailSpecification).toContainText(/Attachments.*Supported/)
  await expect(detailSpecification).toContainText(/Temperature.*Not supported/)
  await expect(detailDialog).toContainText('Pricing')
  await expect(detailDialog).toContainText('$0.25')
  await detailDialog.getByRole('button', { name: 'Close' }).click()
  await page.getByRole('button', { name: 'Confirm' }).click()
  const savedDestination = page.getByRole('button', { name: 'Edit destination 1' })
  await expect(savedDestination).toContainText('Provider A')
  await expect(savedDestination).toContainText('gpt-available')
  await expect(page.getByRole('group', { name: 'Model specification' })).toContainText('1.05M')

  await savedDestination.click({ position: { x: 4, y: 4 } })
  await expect(page.getByRole('dialog')).toBeVisible()
  await page.getByRole('button', { name: 'Confirm' }).click()
  await savedDestination.getByRole('button', { name: /^Token limits:/ }).click()
  await expect(page.getByRole('dialog')).toBeVisible()
  await page.getByRole('button', { name: 'Confirm' }).click()

  await page.getByRole('button', { name: 'Add destination' }).click()
  await page.getByLabel('Destination 2 model service', { exact: true }).click()
  await page.getByRole('option', { name: 'Provider A' }).click()
  await page.getByLabel('Destination 2 model', { exact: true }).click()
  await page.getByRole('option', { name: /GPT Available.*gpt-available/ }).click()
  await page.getByRole('button', { name: 'Confirm' }).click()
  await expect(page.getByRole('button', { name: 'Edit destination 2' })).toContainText('gpt-available')

  await page
    .getByRole('button', { name: 'Edit destination 1' })
    .getByRole('button', { name: /^Token limits:/ })
    .dragTo(page.locator('[data-slot="target-priority-connector"][data-position="empty"]'))
  await expect(page.getByLabel('Layer 1')).toBeVisible()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(page.getByLabel('Destination 1 priority')).toHaveCount(0)

  await savedDestination.click({ position: { x: 4, y: 4 } })
  await expect(page.getByRole('dialog')).toBeVisible()
  await page.getByRole('button', { name: 'Confirm' }).click()
  await savedDestination.focus()
  await page.keyboard.press('Space')
  await expect(page.getByRole('dialog')).toBeVisible()
  await page.getByRole('button', { name: 'Confirm' }).click()

  await page.getByLabel('How requests are sent').click()
  await page.getByRole('option', { name: 'Latency preference' }).click()
  await expect(page.getByLabel('Destination 1 traffic share')).toHaveCount(0)
  await expect(page.getByLabel('Layer 1')).toBeVisible()

  await page.getByRole('button', { name: 'Edit destination 1' }).click()
  await page.getByLabel('Destination 1 model service', { exact: true }).click()
  await page.getByRole('option', { name: 'Provider B' }).click()
  await expect(page.getByLabel('Destination 1 model', { exact: true })).toHaveText(/Choose a model/)
  await expect(page.getByRole('dialog').getByRole('group', { name: 'Model specification' })).toHaveCount(0)
  await page.getByLabel('Destination 1 model', { exact: true }).click()
  await page.getByRole('option', { name: /GPT Available.*gpt-available/ }).click()
  await page.getByRole('button', { name: 'Confirm' }).click()

  await page.getByRole('switch', { name: 'Enabled' }).click()
  await page.getByRole('button', { name: 'Save model' }).click()
  await expect.poll(() => createAttempts).toBe(1)
  const validationError = page.locator('[data-sonner-toast]')
  await expect(validationError).toBeVisible()
  await expect(modelSearch).toHaveValue('gpt-5.4')
  await page.getByRole('button', { name: 'Edit destination 1' }).click()
  await expect(page.getByLabel('Destination 1 model', { exact: true })).toHaveText(/GPT Available/)
  await page.getByRole('button', { name: 'Confirm' }).click()
  await expect(page.getByRole('switch', { name: 'Enabled' })).not.toBeChecked()
  await expect(validationError).toBeHidden({ timeout: 10_000 })
  await page.getByRole('button', { name: 'Save model' }).click()
  await expect.poll(() => createAttempts).toBe(2)
  expect(createBody?.model_id).toBe('gpt-5.4')
  expect(createBody?.display_name).toBe('')
  await expect.poll(() => disableBody?.is_enabled).toBe(false)
  await expect(page).toHaveURL(/\/models$/)
  await expect(page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })).toHaveCount(0)
})

test('Model ID remains editable while the Canonical Model catalog fails to load', async ({ page }) => {
  let releaseCatalog!: () => void
  const catalogGate = new Promise<void>((resolve) => {
    releaseCatalog = resolve
  })
  let createAttempts = 0

  await page.route('**/api/v1/catalog/models', async (route) => {
    await catalogGate
    await route.fulfill({ status: 503, json: { error: 'Catalog unavailable' } })
  })
  await page.route('**/api/v1/providers', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })
  await page.route('**/api/v1/models', async (route) => {
    if (route.request().method() === 'POST') createAttempts += 1
    await route.fulfill({ json: { data: [] } })
  })

  await page.goto('/models/new')
  const modelId = page.getByRole('combobox', { name: 'Model ID', exact: true })
  await expect(modelId).toBeEnabled()
  await modelId.fill('custom/offline-model')
  await modelId.press('Enter')
  await expect(modelId).toHaveValue('custom/offline-model')
  expect(createAttempts).toBe(0)

  releaseCatalog()
  await expect(modelId).toBeEnabled()
  await modelId.fill('custom/after-failure')
  await expect(modelId).toHaveValue('custom/after-failure')
})
