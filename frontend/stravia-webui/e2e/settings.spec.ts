import { expect, test } from '@playwright/test'

import type { MediaUnderstandingConfigView } from '../src/lib/types'
import { prepareApp } from './prepare-app'

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('settings fields align in wide containers and stack in narrow containers', async ({ page }) => {
  await page.setViewportSize({ width: 1500, height: 900 })
  await page.goto('/settings')

  const themeField = page.locator('[data-slot="field"]').filter({ has: page.locator('#theme-preference') })
  const themeLabel = themeField.locator('[data-slot="field-label"]')
  const themeControl = themeField.locator('#theme-preference')
  const proxyUrl = page.locator('#proxy-url')
  const proxyBypass = page.locator('#proxy-bypass')
  const retention = page.locator('#log-retention')

  const wideThemeLabelBox = await themeLabel.boundingBox()
  const wideThemeControlBox = await themeControl.boundingBox()
  const wideProxyUrlBox = await proxyUrl.boundingBox()
  const wideProxyBypassBox = await proxyBypass.boundingBox()
  const wideRetentionBox = await retention.boundingBox()
  expect(wideThemeLabelBox).not.toBeNull()
  expect(wideThemeControlBox).not.toBeNull()
  expect(wideProxyUrlBox).not.toBeNull()
  expect(wideProxyBypassBox).not.toBeNull()
  expect(wideRetentionBox).not.toBeNull()
  expect(wideThemeLabelBox!.x + wideThemeLabelBox!.width).toBeLessThan(wideThemeControlBox!.x)
  expect(Math.abs(wideProxyUrlBox!.x - wideProxyBypassBox!.x)).toBeLessThan(1)
  expect(Math.abs(wideProxyUrlBox!.width - wideProxyBypassBox!.width)).toBeLessThan(1)
  expect(
    Math.abs(wideProxyUrlBox!.x + wideProxyUrlBox!.width - (wideRetentionBox!.x + wideRetentionBox!.width)),
  ).toBeLessThan(1)
  await expect(themeField.locator('[data-slot="field-hint"], [data-slot="field-hint-text"]')).toHaveCount(0)
  await expect(page.getByText('Theme and language apply immediately and persist on this device.')).toHaveCount(0)
  await expect(page.getByText('Following system uses your operating system theme preference.')).toHaveCount(0)
  await expect(page.getByText('Choose the language Stravia uses.')).toHaveCount(0)

  const proxyLabelBox = await page.locator('label[for="proxy-url"]').boundingBox()
  expect(proxyLabelBox).not.toBeNull()
  expect(
    Math.abs(proxyLabelBox!.y + proxyLabelBox!.height / 2 - (wideProxyUrlBox!.y + wideProxyUrlBox!.height / 2)),
  ).toBeLessThan(1)

  await page.setViewportSize({ width: 500, height: 900 })

  const narrowThemeLabelBox = await themeLabel.boundingBox()
  const narrowThemeControlBox = await themeControl.boundingBox()
  const narrowProxyUrlBox = await proxyUrl.boundingBox()
  const narrowProxyBypassBox = await proxyBypass.boundingBox()
  const narrowRetentionBox = await retention.boundingBox()
  expect(narrowThemeLabelBox).not.toBeNull()
  expect(narrowThemeControlBox).not.toBeNull()
  expect(narrowProxyUrlBox).not.toBeNull()
  expect(narrowProxyBypassBox).not.toBeNull()
  expect(narrowRetentionBox).not.toBeNull()
  expect(narrowThemeLabelBox!.y + narrowThemeLabelBox!.height).toBeLessThanOrEqual(narrowThemeControlBox!.y)
  expect(Math.abs(narrowProxyUrlBox!.x - narrowProxyBypassBox!.x)).toBeLessThan(1)
  expect(Math.abs(narrowProxyUrlBox!.width - narrowProxyBypassBox!.width)).toBeLessThan(1)
  expect(Math.abs(narrowProxyUrlBox!.width - narrowRetentionBox!.width)).toBeLessThan(1)
  await expect(page.getByRole('spinbutton', { name: 'Retention period (days)', exact: true })).toBeVisible()
})

test('web search and media understanding use the settings content width', async ({ page }) => {
  await page.setViewportSize({ width: 1600, height: 900 })

  await page.goto('/settings')
  const settingsWidth = (await page.locator('.route-page').boundingBox())?.width
  expect(settingsWidth).toBeGreaterThan(0)

  await page.goto('/web-search')
  expect((await page.locator('.route-page').boundingBox())?.width).toBe(settingsWidth)

  await page.goto('/media-understanding')
  expect((await page.locator('.route-page').boundingBox())?.width).toBe(settingsWidth)
})

test('advanced features keep separate media and web search surfaces', async ({ page }) => {
  const searchConfig = {
    revision: 3,
    enabled: true,
    backend: { kind: 'local', model_id: 'model-search' },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-08-17T00:00:00Z',
    limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
  }
  const mediaConfig = {
    enabled: true,
    model_id: 'model-media',
    thinking_level: 'high',
    state: 'available',
    eligible_models: [
      {
        id: 'model-media',
        model_id: 'multimodal-model',
        display_name: 'Multimodal model',
        supported_thinking_levels: ['off', 'medium', 'high'],
      },
    ],
  } satisfies MediaUnderstandingConfigView

  await page.route('**/api/v1/web-search/config', async (route) => {
    await route.fulfill({ json: { data: searchConfig } })
  })
  await page.route('**/api/v1/web-search/eligible-models', async (route) => {
    await route.fulfill({
      json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] },
    })
  })
  await page.route('**/api/v1/web-search/codex-providers', async (route) => {
    await route.fulfill({
      json: { data: [{ id: 'provider-codex', name: 'Codex account', models: [{ id: 'gpt-5' }] }] },
    })
  })
  await page.route('**/api/v1/media-understanding', async (route) => {
    await route.fulfill({ json: { data: mediaConfig } })
  })
  await page.route('**/api/v1/web-access/settings', async (route) => {
    await route.fulfill({ json: { data: { enabled: true, search_provider_ids: [], fetch_provider_ids: [] } } })
  })
  await page.route('**/api/v1/web-providers', async (route) => {
    await route.fulfill({ json: { data: [] } })
  })

  await page.goto('/media-understanding')
  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  await expect(navigation.getByText('Advanced Features', { exact: true })).toBeVisible()
  await expect(navigation.getByRole('link', { name: 'Media understanding' })).toBeVisible()
  await expect(navigation.getByRole('link', { name: 'Web search' })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Media understanding', exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Understanding model' })).toBeVisible()
  await expect(page.getByText('Available', { exact: true })).toBeVisible()
  await expect(page.locator('#media-model')).toHaveText('Multimodal model')
  await page.locator('#media-model').click()
  await expect(page.getByText('multimodal-model', { exact: true })).toBeVisible()
  await page.getByRole('option', { name: 'Multimodal model' }).click()
  await expect(page.locator('#media-thinking-level')).toHaveText('high')
  await expect(page.getByRole('heading', { name: 'Supported images and limits' })).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Image processing' })).toHaveCount(0)

  await navigation.getByRole('link', { name: 'Web search' }).click()
  await expect(page).toHaveURL(/\/web-search$/)
  await expect(page.getByRole('heading', { level: 1, name: 'Web search', exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Web search and page access' })).toBeVisible()
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Local search limits' })).toBeVisible()
  const localTurns = page.locator('#search-max-turns')
  await localTurns.fill('9')

  await page.locator('#search-backend').click()
  await page.getByRole('option', { name: 'Use Codex web search' }).click()
  await expect(page.getByText('Codex account', { exact: true })).toBeVisible()
  await expect(page.getByText('Codex model', { exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Web search and page access' })).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Local search limits' })).toHaveCount(0)
  await expect(page.getByText(/Search terms and URLs are sent/)).toHaveCount(0)

  await page.locator('#search-backend').click()
  await page.getByRole('option', { name: 'Use a Stravia model' }).click()
  await expect(page.locator('#search-local-model')).toHaveText('Search model')
  await page.locator('#search-local-model').click()
  await expect(page.getByText('search-model', { exact: true })).toBeVisible()
  await page.getByRole('option', { name: 'Search model' }).click()
  await expect(localTurns).toHaveValue('9')
})

test('server update notification skips one version without hiding Settings or exposing download', async ({
  page,
}) => {
  const releaseUrl = 'https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.2.0'
  let skipped = false
  await page.addInitScript(() => {
    window.open = (url) => {
      sessionStorage.setItem('opened-release-url', String(url))
      return null
    }
  })
  await page.route('**/api/v1/updates**', async (route) => {
    if (route.request().method() === 'PUT') {
      skipped = route.request().postDataJSON()?.version === '1.2.0'
    }
    await route.fulfill({
      json: {
        data: {
          current_version: '1.0.0',
          check_status: 'available',
          last_success_at: '2026-09-05T00:00:00Z',
          last_failure: null,
          available_update: {
            version: '1.2.0',
            published_at: '2026-09-04T00:00:00Z',
            release_url: releaseUrl,
            manifest_url:
              'https://github.com/Stravia-AI/StraviaPlatform/releases/download/v1.2.0/stravia-updater.json',
            download_available: true,
            download_error: null,
          },
          skipped,
          download_supported: false,
        },
      },
    })
  })

  await page.goto('/')
  await expect(page.getByText('A Stravia update is available')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Download update' })).toHaveCount(0)
  await page.getByRole('button', { name: 'View release notes' }).click()
  expect(await page.evaluate(() => sessionStorage.getItem('opened-release-url'))).toBe(releaseUrl)
  await page.getByRole('button', { name: 'Skip this version' }).click()
  await expect(page.getByText('A Stravia update is available')).toHaveCount(0)

  await page.goto('/settings')
  await expect(page.getByRole('heading', { name: 'Updates' })).toBeVisible()
  await expect(page.getByText('1.2.0', { exact: true })).toBeVisible()
  await expect(page.getByText('Automatic notifications are skipped for this version.')).toBeVisible()
  await expect(page.getByRole('button', { name: 'Download update' })).toHaveCount(0)
})
