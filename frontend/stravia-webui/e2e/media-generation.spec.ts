import { expect, test } from '@playwright/test'

import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

for (const enabled of [false, true]) {
  test(`saving no image route releases a previously opened model (media enabled: ${enabled})`, async ({ page }) => {
    let config = { enabled, image: { route_id: 'historical-image-route' as string | null } }
    const model = {
      id: 'historical-image-route',
      model_id: 'historical-image-route',
      display_name: 'Historical image route',
      balance: 'traffic_equalization',
      output_modality: null,
      image_generation_bound: true,
      is_enabled: true,
      supported_thinking_levels: [],
      targets: [
        {
          id: 'historical-target',
          provider_id: 'text-provider',
          model: 'text-model',
          enabled: true,
          priority: 0,
          first_token_timeout_ms: 60000,
          target_retry_budget: 0,
          target_cooldown_ms: 60000,
          thinking_level_map: [],
          route_output_modalities: ['text'],
          output_modality_mismatch: true,
        },
      ],
    }
    let savedOutput: string | undefined
    let modelReads = 0
    let releaseModelRefresh!: () => void
    const modelRefresh = new Promise<void>((resolve) => {
      releaseModelRefresh = resolve
    })
    await page.route('**/api/v1/providers', (route) =>
      route.fulfill({
        json: {
          data: [{ id: 'text-provider', name: 'Text service', protocol: 'openai-compatible', is_enabled: true }],
        },
      }),
    )
    await page.route('**/api/v1/providers/text-provider/models', (route) =>
      route.fulfill({
        json: {
          data: {
            models: [{ id: 'text-model', name: 'Text model', available: true, route_output_modalities: ['text'] }],
          },
        },
      }),
    )
    await page.route('**/api/v1/models', (route) =>
      route.fulfill({
        json: { data: [{ ...model, image_generation_bound: config.image.route_id === model.model_id }] },
      }),
    )
    await page.route('**/api/v1/models/historical-image-route', async (route) => {
      if (route.request().method() === 'GET') {
        if (++modelReads > 1) await modelRefresh
      } else {
        const input = route.request().postDataJSON()
        if (config.image.route_id) {
          await route.fulfill({ status: 400, json: { code: 'ROUTE_IMAGE_GENERATION_BOUND' } })
          return
        }
        savedOutput = input.output_modality
      }
      await route.fulfill({
        json: { data: { ...model, image_generation_bound: config.image.route_id === model.model_id } },
      })
    })
    await page.route('**/api/v1/models/historical-image-route/target-statuses', (route) =>
      route.fulfill({ json: { data: [] } }),
    )
    await page.route('**/api/v1/media-generation/config', async (route) => {
      if (route.request().method() === 'PUT') config = route.request().postDataJSON()
      await route.fulfill({
        json: {
          data: {
            config,
            validation: {
              valid: false,
              code: config.image.route_id ? 'route_output_modality_unconfigured' : 'media_generation_route_missing',
              message: null,
            },
          },
        },
      })
    })
    await page.route('**/api/v1/media-generation/eligible-routes', (route) => route.fulfill({ json: { data: [] } }))

    await page.goto('/models/historical-image-route')
    await page.getByRole('button', { name: 'Output type', exact: true }).click()
    await page.getByRole('option', { name: 'Text', exact: true }).click()
    await page.getByRole('link', { name: 'Open media generation', exact: true }).click()
    await page.getByRole('button', { name: 'Discard changes', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Image route', exact: true })).toContainText('historical-image-route')
    // 取消不改变配置；保存才解绑并关闭功能。
    const sheet = page.locator('[data-slot="sheet-content"]')
    await page.getByRole('button', { name: 'Clear image route', exact: true }).click()
    await expect(sheet.getByText('Unlinking the image route also turns off media generation.')).toBeVisible()
    await sheet.getByRole('button', { name: 'Cancel', exact: true }).click()
    await expect(sheet).toHaveCount(0)
    expect(config).toEqual({ enabled, image: { route_id: 'historical-image-route' } })
    await page.getByRole('button', { name: 'Clear image route', exact: true }).click()
    await sheet.getByRole('button', { name: 'Unlink route', exact: true }).click()
    await expect.poll(() => config).toEqual({ enabled: false, image: { route_id: null } })
    await expect(sheet).toHaveCount(0)
    await expect(page.getByRole('switch', { name: 'Enable media generation' })).not.toBeChecked()

    await page
      .getByRole('navigation', { name: 'Primary navigation' })
      .getByRole('link', { name: 'Models', exact: true })
      .click()
    await page.getByRole('row').filter({ hasText: 'Historical image route' }).click()
    await page.getByRole('button', { name: 'Output type', exact: true }).click()
    await page.getByRole('option', { name: 'Text', exact: true }).click()
    await page.getByRole('textbox', { name: 'Display name', exact: true }).fill('Recovered draft')
    releaseModelRefresh()
    await expect(page.getByRole('link', { name: 'Open media generation', exact: true })).toHaveCount(0)
    await expect(page.getByRole('textbox', { name: 'Display name', exact: true })).toHaveValue('Recovered draft')
    await expect(page.getByRole('button', { name: 'Output type', exact: true })).toHaveText('Text')
    await page.getByRole('button', { name: 'Save model', exact: true }).click()
    await expect.poll(() => savedOutput).toBe('text')
  })
}

for (const locale of ['en-US', 'zh-CN'] as const) {
  test(`invalid historical media reference can be disabled and unlinked (${locale}, 320px)`, async ({ page }) => {
    await page.addInitScript((language) => localStorage.setItem('stravia-locale', language), locale)
    await page.setViewportSize({ width: 320, height: 780 })
    const zh = locale === 'zh-CN'
    let config = { enabled: true, image: { route_id: 'historical-image-route' as string | null } }
    const writes: (typeof config)[] = []
    await page.route('**/api/v1/media-generation/config', async (route) => {
      if (route.request().method() === 'PUT') {
        config = route.request().postDataJSON()
        writes.push(structuredClone(config))
      }
      await route.fulfill({
        json: {
          data: {
            config,
            validation: {
              valid: false,
              code: 'route_output_modality_unconfigured',
              message: 'Route requires reconfiguration.',
            },
          },
        },
      })
    })
    await page.route('**/api/v1/media-generation/eligible-routes', (route) => route.fulfill({ json: { data: [] } }))
    await page.goto('/media-generation')
    await expect(page.getByText(/historical-image-route/).first()).toBeVisible()
    await page.getByRole('switch', { name: zh ? '启用媒体生成' : 'Enable media generation' }).click()
    await expect.poll(() => writes.length).toBe(1)
    expect(writes[0]).toEqual({ enabled: false, image: { route_id: 'historical-image-route' } })
    await page.getByRole('button', { name: zh ? '清除图片路由' : 'Clear image route', exact: true }).click()
    const sheet = page.locator('[data-slot="sheet-content"]')
    await expect(sheet).toBeVisible()
    await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
    expect(writes).toHaveLength(1)
    await sheet.getByRole('button', { name: zh ? '解除路由' : 'Unlink route', exact: true }).click()
    await expect.poll(() => writes.length).toBe(2)
    expect(writes[1]).toEqual({ enabled: false, image: { route_id: null } })
    await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}

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
  await expect(page.getByRole('heading', { level: 1, name: 'Media generation', exact: true })).toBeVisible()
  await page
    .getByRole('navigation', { name: 'Primary navigation' })
    .getByRole('link', { name: /Media generation/ })
    .click()
  await expect(page).toHaveURL(/\/media-generation$/)
  await expect(page.getByRole('button', { name: 'Save settings', exact: true })).toHaveCount(0)

  // 完整有效的路由选择即时保存；被拒绝的选择与错误一并保留。
  const routeSelect = page.getByRole('button', { name: 'Image route', exact: true })
  await routeSelect.click()
  await page.getByRole('option', { name: 'Image Route B' }).click()
  await expect(page.getByText(/not compatible with image generation/i)).toBeVisible()
  await expect(routeSelect).toContainText('Image Route B')
  expect(config).toEqual({ enabled: false, image: { route_id: 'route-a' } })

  // 独立启停只提交服务器确认的配置，不提交也不丢弃被拒绝的选择。
  await page.getByRole('switch', { name: 'Enable media generation' }).click()
  await expect.poll(() => writes.length).toBe(2)
  expect(writes[1]).toEqual({ enabled: true, image: { route_id: 'route-a' } })
  await expect(page.getByRole('switch', { name: 'Enable media generation' })).toBeChecked()
  await expect(routeSelect).toContainText('Image Route B')
  await expect(page.getByText(/not compatible with image generation/i)).toBeVisible()

  // 带着未保存的选择离开需要确认；保留编辑后选择仍在。
  const discard = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })
  await page
    .getByRole('navigation', { name: 'Primary navigation' })
    .getByRole('link', { name: 'Models', exact: true })
    .click()
  await expect(discard).toBeVisible()
  await discard.getByRole('button', { name: 'Keep editing' }).click()
  await expect(page).toHaveURL(/\/media-generation$/)
  await expect(routeSelect).toContainText('Image Route B')

  await page.getByRole('button', { name: 'Save settings', exact: true }).click()
  await expect(page.getByText('Media generation settings saved.')).toBeVisible()
  await expect.poll(() => config).toEqual({ enabled: true, image: { route_id: 'route-b' } })
  await expect(page.getByText(/not compatible with image generation/i)).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Save settings', exact: true })).toHaveCount(0)
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
  await expect(page.getByRole('link', { name: '添加模型' })).toBeVisible()
  await expect(page.getByRole('button', { name: '保存设置' })).toHaveCount(0)
  const gateHint = page.getByRole('switch', { name: '启用媒体生成', exact: true })
  await gateHint.focus()
  await expect(page.getByRole('tooltip')).toContainText('请先选择并保存图片路由')
  await expect(gateHint).toHaveAccessibleDescription(/请先选择并保存图片路由/)
  await page.keyboard.press('Escape')
  await expect(page.getByRole('tooltip')).toHaveCount(0)
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
})

test.describe('media generation switch guidance', () => {
  test.use({ hasTouch: true })

  test('a blocked switch explains its prerequisite without changing configuration and recovers after saving', async ({
    page,
  }) => {
    let config = { enabled: false, image: { route_id: null as string | null } }
    const writes: (typeof config)[] = []
    await page.route('**/api/v1/media-generation/config', async (route) => {
      if (route.request().method() === 'PUT') {
        config = route.request().postDataJSON()
        writes.push(structuredClone(config))
      }
      await route.fulfill({
        json: {
          data: {
            config,
            validation: {
              valid: Boolean(config.image.route_id),
              code: config.image.route_id ? null : 'media_generation_route_missing',
              message: null,
            },
          },
        },
      })
    })
    await page.route('**/api/v1/media-generation/eligible-routes', (route) =>
      route.fulfill({ json: { data: [{ id: 'image', name: 'Image model' }] } }),
    )
    await page.goto('/media-generation')

    const gate = page.getByRole('switch', { name: 'Enable media generation' })
    const tooltip = page.getByRole('tooltip')
    await expect(gate).toBeDisabled()
    await expect(tooltip).toHaveCount(0)
    await gate.hover()
    await expect(tooltip).toContainText('Choose and save an image route')
    await page.keyboard.press('Escape')
    await expect(tooltip).toHaveCount(0)
    const bounds = (await gate.boundingBox())!
    await page.touchscreen.tap(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2)
    await expect(tooltip).toContainText('Choose and save an image route')
    expect(writes).toEqual([])

    await gate.focus()
    await expect(tooltip).toContainText('Choose and save an image route')
    await expect(gate).toHaveAccessibleDescription(/Choose and save an image route/)
    await page.keyboard.press('Escape')
    await expect(tooltip).toHaveCount(0)

    await page.getByRole('button', { name: 'Image route', exact: true }).click()
    await page.getByRole('option', { name: 'Image model' }).click()
    await expect(gate).toBeEnabled()
    await gate.hover()
    await expect(tooltip).toHaveCount(0)
    await gate.click()
    await expect(gate).toBeChecked()
    expect(writes).toEqual([
      { enabled: false, image: { route_id: 'image' } },
      { enabled: true, image: { route_id: 'image' } },
    ])
  })
})

for (const width of [2560, 390]) {
  test(`media settings column stays within 60rem and aligned with the title at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 })
    await page.route('**/api/v1/media-generation/config', (route) =>
      route.fulfill({
        json: {
          data: {
            config: { enabled: false, image: { route_id: 'image' } },
            validation: { valid: true, code: null, message: null },
          },
        },
      }),
    )
    await page.route('**/api/v1/media-generation/eligible-routes', (route) =>
      route.fulfill({ json: { data: [{ id: 'image', name: 'Image model' }] } }),
    )
    await page.goto('/media-generation')
    const title = page.getByRole('heading', { level: 1, name: 'Media generation', exact: true })
    await expect(title).toBeVisible()
    const gate = page.getByRole('switch', { name: 'Enable media generation' })
    await expect(gate).toBeVisible()
    const column = (await page.locator('.content-column').first().boundingBox())!
    expect(column.width).toBeLessThanOrEqual(960)
    expect(Math.abs(column.x - (await title.boundingBox())!.x)).toBeLessThan(2)
    expect(await page.locator('header').filter({ has: title }).getByRole('switch').count()).toBe(0)
    await expect(page.getByRole('button', { name: 'Save settings', exact: true })).toHaveCount(0)
    await expect(page.getByRole('heading', { name: 'Supported media' })).toHaveCount(0)
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}
