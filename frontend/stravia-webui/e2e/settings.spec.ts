import { expect, test } from '@playwright/test'

import type {
  CredentialRuleCatalog,
  MediaUnderstandingConfigView,
  WebAccessSettings,
  WebProvider,
} from '../src/lib/types'
import { prepareApp } from './prepare-app'

const searchSource: WebProvider = {
  id: 'source-exa',
  name: 'Search source',
  kind: 'exa',
  use_proxy: false,
  capabilities: { search: true, fetch: true },
  created_at: '2026-09-01T00:00:00Z',
  updated_at: '2026-09-01T00:00:00Z',
}

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
    await route.fulfill({ json: { data: { search_provider_ids: [], fetch_provider_ids: [] } } })
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
  await expect(page.locator('#media-model')).toHaveText('Multimodal model')
  await page.locator('#media-model').click()
  await expect(page.getByText('multimodal-model', { exact: true })).toBeVisible()
  await page.getByRole('option', { name: 'Multimodal model' }).click()
  await expect(page.locator('#media-thinking-level')).toHaveText('high')

  await navigation.getByRole('link', { name: 'Web search' }).click()
  await expect(page).toHaveURL(/\/web-search$/)
  await expect(page.getByRole('heading', { level: 1, name: 'Web search', exact: true })).toBeVisible()
  await expect(page.locator('#web-search-sources')).toBeVisible()
  await expect(page.getByRole('switch')).toHaveCount(1)
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Local search limits' })).toBeVisible()
  const localTurns = page.locator('#search-max-turns')
  await localTurns.fill('9')

  await page.locator('#search-backend').click()
  await page.getByRole('option', { name: 'Use Codex web search' }).click()
  await expect(page.getByText('Codex account', { exact: true })).toBeVisible()
  await expect(page.getByText('Codex model', { exact: true })).toBeVisible()
  await expect(page.locator('#web-search-sources')).toHaveCount(0)
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

test('media settings can repair an unavailable binding and preserve the draft after a failed save', async ({
  page,
}) => {
  let rejectSave = true
  let config: MediaUnderstandingConfigView = {
    enabled: true,
    model_id: 'media-fixture',
    thinking_level: 'high',
    state: 'unavailable',
    eligible_models: [
      {
        id: 'media-fixture',
        model_id: 'vision-fixture',
        display_name: 'Fixture vision',
        supported_thinking_levels: ['medium'],
      },
    ],
  }
  const submitted: unknown[] = []
  await page.route('**/api/v1/media-understanding', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      submitted.push(input)
      if (rejectSave) {
        await route.fulfill({ status: 503, json: { error: 'Fixture media save unavailable' } })
        return
      }
      config = { ...config, ...input, state: 'available' }
    }
    await route.fulfill({ json: { data: config } })
  })
  await page.goto('/media-understanding')
  await expect(page.locator('#media-thinking-level')).toHaveText('medium')
  const save = page.getByRole('button', { name: 'Save settings', exact: true })
  const unsaved = page.getByRole('status').filter({ hasText: 'Unsaved changes' })
  await expect(unsaved).toBeVisible()
  await expect(save).toBeEnabled()
  await save.click()
  await expect(page.getByRole('alert').filter({ hasText: 'Fixture media save unavailable' })).toBeVisible()
  await expect(page.locator('#media-model')).toHaveText('Fixture vision')
  await expect(page.locator('#media-thinking-level')).toHaveText('medium')
  await expect(unsaved).toBeVisible()
  rejectSave = false
  await save.click()
  await expect(unsaved).toHaveCount(0)
  await expect(save).toBeDisabled()
  expect(submitted).toEqual([
    { enabled: true, model_id: 'media-fixture', thinking_level: 'medium' },
    { enabled: true, model_id: 'media-fixture', thinking_level: 'medium' },
  ])
})

test('media activation saves only confirmed configuration without discarding the model draft', async ({ page }) => {
  let config: MediaUnderstandingConfigView = {
    enabled: true,
    model_id: 'media-fixture',
    thinking_level: 'high',
    state: 'available',
    eligible_models: [
      {
        id: 'media-fixture',
        model_id: 'vision-fixture',
        display_name: 'Fixture vision',
        supported_thinking_levels: ['medium', 'high'],
      },
    ],
  }
  const submitted: unknown[] = []
  await page.route('**/api/v1/media-understanding', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      submitted.push(input)
      config = { ...config, ...input }
    }
    await route.fulfill({ json: { data: config } })
  })
  await page.goto('/media-understanding')
  await page.locator('#media-thinking-level').click()
  await page.getByRole('option', { name: 'medium', exact: true }).click()
  const unsaved = page.getByRole('status').filter({ hasText: 'Unsaved changes' })
  await expect(unsaved).toBeVisible()
  const toggle = page.getByRole('switch')
  await toggle.click()
  await expect(toggle).not.toBeChecked()
  await expect(toggle).toBeEnabled()
  expect(submitted).toEqual([{ enabled: false, model_id: 'media-fixture', thinking_level: 'high' }])
  await expect(page.locator('#media-thinking-level')).toHaveText('medium')
  await expect(unsaved).toBeVisible()
  await page.getByRole('button', { name: 'Save settings', exact: true }).click()
  await expect(unsaved).toHaveCount(0)
  expect(submitted).toEqual([
    { enabled: false, model_id: 'media-fixture', thinking_level: 'high' },
    { enabled: false, model_id: 'media-fixture', thinking_level: 'medium' },
  ])
  await page.reload()
  await expect(toggle).not.toBeChecked()
  await expect(page.locator('#media-thinking-level')).toHaveText('medium')
})

test('search activation saves only confirmed configuration without discarding the limits draft', async ({ page }) => {
  let config = {
    revision: 3,
    enabled: true,
    backend: { kind: 'local', model_id: 'model-search' },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-08-17T00:00:00Z',
    limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
  }
  const submitted: unknown[] = []
  await page.route('**/api/v1/web-search/config', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      submitted.push(input)
      config = { ...config, ...input, revision: config.revision + 1 }
    }
    await route.fulfill({ json: { data: config } })
  })
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({ json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] } }),
  )
  await page.route('**/api/v1/web-access/settings', (route) =>
    route.fulfill({
      json: { data: { search_provider_ids: [searchSource.id], fetch_provider_ids: [searchSource.id] } },
    }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [searchSource] } }))
  await page.goto('/web-search')
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  const turns = page.locator('#search-max-turns')
  await turns.fill('9')
  const unsaved = page.getByRole('status').filter({ hasText: 'Unsaved changes' })
  await expect(unsaved).toBeVisible()
  const toggle = page.getByRole('switch', { name: 'Enable web search', exact: true })
  await toggle.click()
  await expect(toggle).not.toBeChecked()
  await expect(toggle).toBeEnabled()
  expect(submitted).toEqual([
    {
      revision: 3,
      enabled: false,
      backend: { kind: 'local', model_id: 'model-search' },
      max_turns: 6,
      total_time_seconds: 180,
      updated_at: '2026-08-17T00:00:00Z',
    },
  ])
  await expect(turns).toHaveValue('9')
  await expect(unsaved).toBeVisible()
  await page.getByRole('button', { name: 'Save settings', exact: true }).click()
  await expect(unsaved).toHaveCount(0)
  expect(submitted).toHaveLength(2)
  expect(submitted[1]).toMatchObject({ revision: 4, enabled: false, max_turns: 9 })
  await page.reload()
  await expect(toggle).not.toBeChecked()
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  await expect(turns).toHaveValue('9')
})

test('unloaded settings stay non-editable until a failed baseline is recovered', async ({ page }) => {
  let unavailable = true
  await page.route('**/api/v1/settings/proxy_enabled', async (route) => {
    await route.fulfill(
      unavailable ? { status: 503, json: { error: 'Fixture settings unavailable' } } : { json: { data: 'true' } },
    )
  })
  await page.goto('/settings')
  await expect(page.getByRole('alert').filter({ hasText: 'Fixture settings unavailable' })).toBeVisible()
  await expect(page.locator('#proxy-enabled')).toHaveCount(0)
  await expect(page.locator('#proxy-url')).toHaveCount(0)
  await expect(page.locator('#log-retention')).toBeVisible()
  unavailable = false
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(page.locator('#proxy-enabled')).toBeChecked()
  await expect(page.locator('#proxy-url')).toBeVisible()
})

test('source save failures preserve selection and drafts before the single search switch can enable', async ({
  page,
}) => {
  let rejectSave = true
  let saved: WebAccessSettings = { search_provider_ids: [], fetch_provider_ids: [] }
  let config = {
    revision: 1,
    enabled: false,
    backend: { kind: 'local', model_id: 'model-search' },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-09-01T00:00:00Z',
    limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
  }
  await page.route('**/api/v1/web-search/config', async (route) => {
    if (route.request().method() === 'PUT') {
      config = { ...config, ...route.request().postDataJSON(), revision: config.revision + 1 }
    }
    await route.fulfill({ json: { data: config } })
  })
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({ json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [searchSource] } }))
  await page.route('**/api/v1/web-access/settings', async (route) => {
    if (route.request().method() === 'PUT') {
      if (rejectSave) {
        await route.fulfill({ status: 503, json: { error: 'Fixture source save unavailable' } })
        return
      }
      saved = route.request().postDataJSON()
    }
    // An obsolete disabled field in cached data must not create a second activation step.
    await route.fulfill({ json: { data: { ...saved, enabled: false } } })
  })
  await page.goto('/web-search')
  const searchSwitch = page.getByRole('switch')
  const searchSourceCheckbox = page.locator('#web-access-search-source-exa')
  const pageSourceCheckbox = page.locator('#web-access-fetch-source-exa')
  await expect(searchSwitch).toHaveCount(1)
  await expect(searchSwitch).toBeDisabled()
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  const draftTurns = page.locator('#search-max-turns')
  await draftTurns.fill('9')
  await searchSourceCheckbox.click()
  await expect(page.locator('#web-search-sources').getByRole('alert')).toBeVisible()
  await expect(searchSourceCheckbox).not.toBeChecked()
  await expect(draftTurns).toHaveValue('9')
  rejectSave = false
  await searchSourceCheckbox.click()
  await expect(searchSourceCheckbox).toBeChecked()
  await expect(searchSwitch).toBeDisabled()
  await pageSourceCheckbox.click()
  await expect(pageSourceCheckbox).toBeChecked()
  await expect(searchSwitch).toBeEnabled()
  await expect(searchSwitch).not.toBeChecked()
  await searchSwitch.click()
  await expect(searchSwitch).toBeChecked()
  await expect(draftTurns).toHaveValue('9')
  expect(config.max_turns).toBe(6)
  expect(saved).toEqual({ search_provider_ids: [searchSource.id], fetch_provider_ids: [searchSource.id] })
  await page.reload()
  await expect(searchSwitch).toHaveCount(1)
  await expect(searchSwitch).toBeChecked()
  await expect(searchSourceCheckbox).toBeChecked()
  await expect(pageSourceCheckbox).toBeChecked()
  await searchSwitch.click()
  await pageSourceCheckbox.click()
  await expect(searchSwitch).toBeDisabled()
})

test('Codex search activation does not depend on Local sources', async ({ page }) => {
  let config = {
    revision: 1,
    enabled: false,
    backend: { kind: 'codex', provider_id: 'provider-codex', upstream_model: 'gpt-5' },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-09-01T00:00:00Z',
    limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
  }
  await page.route('**/api/v1/web-search/config', async (route) => {
    if (route.request().method() === 'PUT') {
      config = { ...config, ...route.request().postDataJSON(), revision: config.revision + 1 }
    }
    await route.fulfill({ json: { data: config } })
  })
  await page.route('**/api/v1/web-search/codex-providers', (route) =>
    route.fulfill({ json: { data: [{ id: 'provider-codex', name: 'Codex account', models: [{ id: 'gpt-5' }] }] } }),
  )
  await page.route('**/api/v1/web-access/**', (route) =>
    route.fulfill({ status: 503, json: { error: 'Fixture Local sources unavailable' } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [] } }))
  await page.goto('/web-search')
  const searchSwitch = page.getByRole('switch')
  await expect(searchSwitch).toHaveCount(1)
  await expect(searchSwitch).toBeEnabled()
  await searchSwitch.click()
  await expect(searchSwitch).toBeChecked()
  await page.reload()
  await expect(searchSwitch).toBeChecked()
})

test('source load recovery still requires a browser before Local sources can enable search', async ({ page }) => {
  let settingsUnavailable = true
  let saved: WebAccessSettings = { search_provider_ids: [], fetch_provider_ids: [] }
  let browserPath: string | null = null
  const localSource: WebProvider = { ...searchSource, id: 'source-local', name: 'Local source', kind: 'local' }
  let config = {
    revision: 1,
    enabled: false,
    backend: { kind: 'local', model_id: 'model-search' },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-09-01T00:00:00Z',
    limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
  }
  await page.route('**/api/v1/web-search/config', async (route) => {
    if (route.request().method() === 'PUT') {
      config = { ...config, ...route.request().postDataJSON(), revision: config.revision + 1 }
    }
    await route.fulfill({ json: { data: config } })
  })
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({ json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [localSource] } }))
  await page.route('**/api/v1/web-providers/source-local', (route) => route.fulfill({ json: { data: localSource } }))
  await page.route('**/api/v1/web-access/browser', async (route) => {
    if (route.request().method() === 'PUT') browserPath = route.request().postDataJSON().path
    await route.fulfill({
      json: {
        data: {
          configuredPath: browserPath,
          resolvedPath: browserPath,
          source: browserPath ? 'manual' : 'automatic',
          available: browserPath !== null,
          error: null,
        },
      },
    })
  })
  await page.route('**/api/v1/web-access/settings', async (route) => {
    if (settingsUnavailable) {
      await route.fulfill({ status: 503, json: { error: 'Fixture source settings unavailable' } })
      return
    }
    if (route.request().method() === 'PUT') saved = route.request().postDataJSON()
    await route.fulfill({ json: { data: saved } })
  })
  await page.goto('/web-search')
  const searchSwitch = page.getByRole('switch')
  const sources = page.locator('#web-search-sources')
  await expect(searchSwitch).toBeDisabled()
  await expect(sources.getByRole('alert')).toBeVisible()
  await expect(page.locator('#web-access-search-source-local')).toHaveCount(0)
  settingsUnavailable = false
  await sources.getByRole('button', { name: 'Retry', exact: true }).click()
  const searchCheckbox = page.locator('#web-access-search-source-local')
  const fetchCheckbox = page.locator('#web-access-fetch-source-local')
  await expect(searchCheckbox).toBeDisabled()
  await expect(fetchCheckbox).toBeDisabled()
  await expect(searchSwitch).toBeDisabled()
  await sources.getByRole('button', { name: 'Edit', exact: true }).click()
  await page.locator('#web-provider-browser-path').fill('C:\\Browser\\chrome.exe')
  await page.getByRole('dialog').getByRole('button', { name: 'Save service', exact: true }).click()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(searchCheckbox).toBeEnabled()
  await searchCheckbox.click()
  await expect(searchCheckbox).toBeChecked()
  await expect(searchSwitch).toBeDisabled()
  await fetchCheckbox.click()
  await expect(fetchCheckbox).toBeChecked()
  await expect(searchSwitch).toHaveCount(1)

  await expect(searchSwitch).toBeEnabled()
  await searchSwitch.click()
  await expect(searchSwitch).toBeChecked()
})

test('server update notification skips one version without hiding Settings or exposing download', async ({ page }) => {
  const releaseUrl = 'https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.2.0'
  let version = '1.2.0'
  let skippedVersion: string | null = null
  await page.addInitScript(() => {
    window.open = (url) => {
      sessionStorage.setItem('opened-release-url', String(url))
      return null
    }
  })
  await page.route('**/api/v1/updates**', async (route) => {
    if (route.request().method() === 'PUT') {
      skippedVersion = route.request().postDataJSON()?.version ?? null
    }
    await route.fulfill({
      json: {
        data: {
          current_version: '1.0.0',
          check_status: 'available',
          last_success_at: '2026-09-05T00:00:00Z',
          last_failure: null,
          available_update: {
            version,
            published_at: '2026-09-04T00:00:00Z',
            release_url: releaseUrl,
            manifest_url: 'https://github.com/Stravia-AI/StraviaPlatform/releases/download/v1.2.0/stravia-updater.json',
            download_available: true,
            download_error: null,
          },
          skipped: skippedVersion === version,
          download_supported: false,
        },
      },
    })
  })

  await page.clock.install()
  await page.goto('/settings')
  const notificationTitle = page.getByText('A Stravia update is available', { exact: true })
  await expect(notificationTitle).toHaveCount(1)
  await page.clock.fastForward(60_000)
  await expect(notificationTitle).toHaveCount(1)
  await page.getByRole('button', { name: 'Check for updates', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Check for updates', exact: true })).toBeEnabled()
  await page.getByRole('button', { name: 'Check for updates', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Check for updates', exact: true })).toBeEnabled()
  await expect(notificationTitle).toHaveCount(1)
  const notification = page.getByRole('status').filter({ hasText: 'A Stravia update is available' })
  await notification.getByRole('button', { name: 'Close', exact: true }).click()
  await expect(notificationTitle).toHaveCount(0)
  expect(skippedVersion).toBeNull()
  await expect(page.getByRole('button', { name: 'Skip this version', exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'View release notes', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Check for updates', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Check for updates', exact: true })).toBeEnabled()
  await expect(notificationTitle).toHaveCount(0)

  await page.goto('/')
  await expect(notificationTitle).toHaveCount(1)
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
  expect(skippedVersion).toBe('1.2.0')
  await expect(notificationTitle).toHaveCount(0)

  version = '1.3.0'
  await page.goto('/')
  await expect(notificationTitle).toHaveCount(1)
  await expect(notification).toContainText('1.3.0')
  expect(skippedVersion).toBe('1.2.0')
})

test('cached update remains actionable when the latest automatic check fails', async ({ page }) => {
  const releaseUrl = 'https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v1.2.0'
  const status = {
    current_version: '1.0.0',
    check_status: 'error',
    last_success_at: '2026-09-04T00:00:00Z',
    last_failure: {
      code: 'UPDATE_REQUEST_FAILED',
      message: 'Unable to connect to GitHub Releases',
      attempted_at: '2026-09-05T00:00:00Z',
    },
    available_update: {
      version: '1.2.0',
      published_at: '2026-09-04T00:00:00Z',
      release_url: releaseUrl,
      manifest_url: 'https://github.com/Stravia-AI/StraviaPlatform/releases/download/v1.2.0/stravia-updater.json',
      download_available: true,
      download_error: null,
    },
    skipped: false,
    download_supported: false,
  }
  await page.route('**/api/v1/updates**', async (route) => {
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ data: status }) })
  })

  await page.goto('/settings')
  const updateAlerts = page.locator('#updates').getByRole('alert')
  await expect(updateAlerts).toHaveCount(1)
  await expect(updateAlerts).toContainText(status.last_failure.message)
  await expect(page.getByText('1.2.0', { exact: true })).toBeVisible()
  await expect(page.getByRole('button', { name: 'View release notes' })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Download update' })).toHaveCount(0)
})

for (const locale of ['en-US', 'zh-CN']) {
  test(`disabled update checks are informational and cannot be retried (${locale})`, async ({ page }) => {
    const technicalMessage = 'Production update checks are disabled in debug and test builds'
    await page.addInitScript((locale) => localStorage.setItem('stravia-locale', locale), locale)
    await page.route('**/api/v1/updates**', async (route) => {
      await route.fulfill({
        json: {
          data: {
            current_version: '0.1.5',
            check_status: 'error',
            last_success_at: null,
            last_failure: {
              code: 'UPDATE_CHECK_DISABLED',
              message: technicalMessage,
              attempted_at: '2026-09-05T00:00:00Z',
            },
            available_update: null,
            skipped: false,
            download_supported: false,
          },
        },
      })
    })

    await page.goto('/settings')
    const updates = page.locator('#updates')
    await expect(updates.getByRole('button')).toBeDisabled()
    await expect(updates.getByRole('status')).toBeVisible()
    await expect(updates.getByRole('alert')).toHaveCount(0)
    await expect(updates).not.toContainText(technicalMessage)
  })
}

const credentialCatalog: CredentialRuleCatalog = {
  prefilter: 'contains(text, "test_")',
  filter: 'len(secret) > 8',
  rules: Array.from({ length: 35 }, (_, index) => ({
    id: `fixture-rule-${index}`,
    name: `Fixture credential ${index}`,
    target: 'Fixture API credentials',
    description: 'Detects test-only API credentials with local context.',
    regex: 'test_[A-Za-z0-9]+',
    path: index === 34 ? '^\\.env$' : null,
    secret_group: 0,
    keywords: ['test_'],
    filter: 'secret != "test_example"',
    components:
      index === 34
        ? [
            { id: 'fixture-rule-0', within: '5L', optional: false },
            { id: 'fixture-rule-1', within: '7L,200C', optional: true },
          ]
        : [],
    skip_report: index === 34,
    specificity: 10,
    confidence: 'high',
  })),
}

test('credential protection table keeps its header above the draggable body scrollbar', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 })
  await page.route('**/api/v1/reversible-redaction/rules', (route) =>
    route.fulfill({ json: { data: credentialCatalog } }),
  )
  await page.goto('/reversible-redaction')
  const table = page.getByRole('table')
  await expect(table.getByRole('button', { name: /Fixture credential 0/ })).toBeVisible()
  const header = table.locator('thead')
  const scrollArea = page.locator('[data-slot="data-table-viewport"]')
  const viewport = scrollArea.locator('[data-scroll-area-viewport]')
  const main = page.getByRole('main')
  const paginator = page.locator('[data-slot="data-table-paginator"]')
  for (const height of [800, 1039]) {
    await page.setViewportSize({ width: 1280, height })
    await expect
      .poll(async () => {
        const mainBox = (await main.boundingBox())!
        const paginatorBox = (await paginator.boundingBox())!
        const bottomGap = mainBox.y + mainBox.height - paginatorBox.y - paginatorBox.height
        return bottomGap >= 0 && bottomGap <= 24
      })
      .toBe(true)
  }
  await page.setViewportSize({ width: 1280, height: 800 })
  const bar = scrollArea.locator('[data-scroll-area-scrollbar][data-orientation="vertical"]')
  const thumb = bar.locator('[data-scroll-area-thumb]')
  await expect(thumb).toBeVisible()
  const headerBefore = (await header.boundingBox())!
  await expect.poll(async () => (await bar.boundingBox())!.y).toBeCloseTo(headerBefore.y + headerBefore.height, 0)
  const thumbBox = (await thumb.boundingBox())!
  await page.mouse.move(thumbBox.x + thumbBox.width / 2, thumbBox.y + thumbBox.height / 2)
  await page.mouse.down()
  await page.mouse.move(thumbBox.x + thumbBox.width / 2, thumbBox.y + thumbBox.height / 2 + 80, { steps: 8 })
  await page.mouse.up()
  await expect.poll(() => viewport.evaluate((element) => element.scrollTop)).toBeGreaterThan(0)
  expect((await header.boundingBox())!.y).toBeCloseTo(headerBefore.y, 0)
  await expect(table.getByRole('button', { name: /Fixture credential 0$/ })).not.toBeInViewport()
})

for (const locale of ['en-US', 'zh-CN']) {
  test(`credential protection browsing and manual testing work without enabling protection (${locale})`, async ({
    page,
  }) => {
    const zh = locale === 'zh-CN'
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    const submitted: string[] = []
    const cursors: Array<string | null> = []
    await page.route('**/api/v1/reversible-redaction/**', async (route) => {
      const url = new URL(route.request().url())
      if (url.pathname.endsWith('/rules')) {
        await route.fulfill({ json: { data: credentialCatalog } })
      } else if (url.pathname.endsWith('/discoveries')) {
        const cursor = url.searchParams.get('cursor')
        cursors.push(cursor)
        await route.fulfill({
          json: {
            data: {
              items: [
                {
                  interaction_id: cursor ? 'interaction-second' : 'interaction-atlas',
                  api_key_name: cursor ? 'Second fixture key' : 'Fixture key',
                  discovered_at: 1_788_854_400_000,
                  new_credential_count: 2,
                  rule_ids: ['fixture-rule-0'],
                  source_types: ['user_message', 'tool_arguments'],
                  status: 'failed',
                  observation_gap: false,
                },
              ],
              next_cursor: cursor ? null : 'next-fixture',
              observation_gap: false,
            },
          },
        })
      } else {
        expect(route.request().method()).toBe('POST')
        expect(url.search).toBe('')
        const { text } = route.request().postDataJSON() as { text: string }
        submitted.push(text)
        const start = text.indexOf('test_credential')
        await route.fulfill({
          json: {
            data: {
              matches:
                start < 0
                  ? []
                  : [
                      {
                        rule_id: 'fixture-rule-0',
                        start,
                        end: start + 15,
                        start_line: 2,
                        start_column: 3,
                        end_line: 2,
                        end_column: 18,
                      },
                    ],
            },
          },
        })
      }
    })
    await page.goto('/reversible-redaction')
    await expect(page.getByRole('heading', { level: 1, name: zh ? '凭据保护' : 'Credential Protection' })).toBeVisible()
    await expect(page.locator('a[href="/reversible-redaction"]')).toContainText(
      zh ? '凭据保护' : 'Credential Protection',
    )
    await expect(page.getByRole('switch')).not.toBeChecked()
    const tabBar = page.getByRole('tablist')
    await expect.poll(() => tabBar.evaluate((element) => element.scrollHeight <= element.clientHeight)).toBe(true)
    const search = page.locator('#credential-rule-search')
    await expect(page.getByRole('textbox', { name: zh ? '待测文本' : 'Text to test' })).toHaveCount(0)
    await expect(page.getByRole('button', { name: /Fixture credential 34/ })).toHaveCount(0)
    await page.getByRole('button', { name: zh ? '末页' : 'Last page', exact: true }).click()
    await expect(page.getByRole('button', { name: /Fixture credential 34/ })).toBeVisible()
    await search.fill('fixture-rule-34')
    const rule = page.getByRole('button', { name: /Fixture credential 34/ })
    await rule.focus()
    await page.keyboard.press('Enter')
    await expect(page.getByText('test_[A-Za-z0-9]+', { exact: true })).toBeVisible()
    const inspector = page.getByRole('dialog')
    await expect(inspector.getByRole('region', { name: zh ? '排除条件' : 'Exclusions' })).toContainText(
      'secret != "test_example"',
    )
    await expect(inspector.getByRole('region', { name: zh ? '路径限制' : 'Path restriction' })).toContainText(
      '^\\.env$',
    )
    const components = inspector.getByRole('region', { name: zh ? '组合匹配' : 'Combined match' })
    await expect(components.getByRole('listitem').filter({ hasText: 'Fixture credential 0' })).toContainText(
      zh ? '前后 4 行内' : 'Within 4 lines before or after',
    )
    await expect(components.getByRole('listitem').filter({ hasText: 'Fixture credential 0' })).toContainText(
      zh ? '必需' : 'Required',
    )
    await expect(components.getByRole('listitem').filter({ hasText: 'Fixture credential 1' })).toContainText(
      zh
        ? '前后 6 行内 · 主匹配为单行时，前后 200 字节内'
        : 'Within 6 lines before or after · Within 200 bytes before or after a single-line primary match',
    )
    await expect(components.getByRole('listitem').filter({ hasText: 'Fixture credential 1' })).toContainText(
      zh ? '可选' : 'Optional',
    )
    await inspector.getByRole('button', { name: zh ? '规则参数' : 'Rule parameters' }).click()
    await expect(
      inspector.getByRole('definition').filter({ hasText: zh ? '仅作组合条件' : 'Component only' }),
    ).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('dialog')).toHaveCount(0)
    await search.fill('')
    await page.getByRole('tab', { name: zh ? '命中记录' : 'Hit records', exact: true }).click()
    await page.getByRole('button', { name: /Fixture key/ }).click()
    await expect(page.getByText(zh ? '请求状态' : 'Request status', { exact: true })).toBeVisible()
    await expect(page.getByRole('definition').filter({ hasText: zh ? '失败' : 'Failed' })).toBeVisible()
    await expect(
      page.getByRole('link', { name: zh ? '查看关联请求记录' : 'Open request observation' }),
    ).toHaveAttribute('href', '/logs?interaction=interaction-atlas')
    await page.getByRole('button', { name: zh ? '加载更多发现' : 'Load more discoveries' }).click()
    await expect(page.getByRole('button', { name: /Second fixture key/ })).toBeVisible()
    expect(cursors).toEqual([null, 'next-fixture'])
    await page.getByRole('tab', { name: zh ? '匹配测试' : 'Matching test', exact: true }).click()
    const input = page.getByRole('textbox', { name: zh ? '待测文本' : 'Text to test' })
    const text = '说明😀\n前缀test_credential 后文'
    await input.fill(text)
    expect(submitted).toEqual([])
    await page.getByRole('button', { name: zh ? '测试匹配' : 'Test matching', exact: true }).click()
    await expect(page.getByRole('status').filter({ hasText: zh ? '命中 1 处' : '1 matches' })).toBeVisible()
    expect(submitted).toEqual([text])
    const result = page.getByRole('button', { name: /Fixture credential 0.*(?:Line|第)/ })
    await result.focus()
    await page.keyboard.press('Enter')
    expect(
      await input.evaluate((element: HTMLTextAreaElement) =>
        element.value.slice(element.selectionStart, element.selectionEnd),
      ),
    ).toBe('test_credential')
    await input.fill('ordinary test text')
    await expect(result).toHaveCount(0)
    await page.getByRole('button', { name: zh ? '测试匹配' : 'Test matching', exact: true }).click()
    await expect(
      page.getByRole('status').filter({ hasText: zh ? '未匹配到现有规则' : 'No existing rule matched' }),
    ).toBeVisible()
    await page.getByRole('button', { name: zh ? '清空输入与结果' : 'Clear input and results' }).click()
    await expect(input).toHaveValue('')
    await expect(
      page.getByRole('status').filter({ hasText: zh ? '未匹配到现有规则' : 'No existing rule matched' }),
    ).toHaveCount(0)
    for (const width of [500, 320]) {
      await page.setViewportSize({ width, height: 900 })
      await expect
        .poll(() =>
          tabBar.evaluate(
            (element) => element.scrollHeight <= element.clientHeight && element.scrollWidth <= element.clientWidth,
          ),
        )
        .toBe(true)
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
    }
  })
}

test('credential protection distinguishes failed saves, unavailable observations and failed tests', async ({
  page,
}) => {
  let saved = 'false'
  let rejectSave = true
  let saveAttempts = 0
  let releaseSave!: () => void
  const pendingSave = new Promise<void>((resolve) => {
    releaseSave = resolve
  })
  let unavailable = true
  let rejectTest = true
  await page.route('**/api/v1/settings/reversible_redaction_enabled', async (route) => {
    if (route.request().method() === 'PUT') {
      saveAttempts++
      if (saveAttempts === 1) await pendingSave
      if (rejectSave) {
        await route.fulfill({ status: 503, json: { error: 'Fixture save unavailable' } })
        return
      }
      saved = route.request().postDataJSON().value
    }
    await route.fulfill({ json: { data: saved } })
  })
  await page.route('**/api/v1/reversible-redaction/**', async (route) => {
    const path = new URL(route.request().url()).pathname
    if (path.endsWith('/test')) {
      await route.fulfill(
        rejectTest ? { status: 503, json: { error: 'Fixture detector failure' } } : { json: { data: { matches: [] } } },
      )
    } else if (unavailable) {
      await route.fulfill({ status: 503, json: { error: 'Fixture observation unavailable' } })
    } else if (path.endsWith('/rules')) {
      await route.fulfill({ json: { data: credentialCatalog } })
    } else {
      await route.fulfill({ json: { data: { items: [], next_cursor: null, observation_gap: true } } })
    }
  })
  await page.goto('/reversible-redaction')
  const toggle = page.getByRole('switch')
  await expect(toggle).not.toBeChecked()
  await toggle.click()
  await expect.poll(() => saveAttempts).toBe(1)
  await expect(toggle).toBeDisabled()
  await expect(toggle).not.toBeChecked()
  await toggle.click({ force: true })
  expect(saveAttempts).toBe(1)
  releaseSave()
  const saveError = page.getByRole('alert').filter({ hasText: 'Fixture save unavailable' })
  await expect(saveError).toBeVisible()
  await expect(toggle).not.toBeChecked()
  await expect(toggle).toBeEnabled()
  await page.getByRole('tab', { name: 'Matching test', exact: true }).click()
  await expect(saveError).toBeVisible()
  await page.getByRole('tab', { name: /Current rules/ }).click()
  rejectSave = false
  await toggle.click()
  await expect(toggle).toBeChecked()
  await expect(toggle).toBeEnabled()
  await expect(saveError).toHaveCount(0)
  expect(saveAttempts).toBe(2)
  expect(saved).toBe('true')
  const catalog = page.getByRole('region', { name: 'Current rules' })
  const timeline = page.getByRole('region', { name: 'Recent discoveries' })
  await expect(catalog.getByRole('alert')).toContainText('Rules could not be loaded.', { timeout: 15_000 })
  unavailable = false
  await catalog.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(catalog.locator('#credential-rule-search')).toBeVisible()
  await page.getByRole('tab', { name: 'Hit records', exact: true }).click()
  await expect(timeline.getByRole('alert')).toContainText('Discovery observations are unavailable.')
  await expect(timeline.getByText('No new credential discoveries in the available observations.')).toHaveCount(0)
  await timeline.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(timeline.getByRole('alert')).toContainText('Observation data is incomplete.')
  await expect(timeline.getByText('No new credential discoveries in the available observations.')).toHaveCount(0)
  await page.getByRole('tab', { name: 'Matching test', exact: true }).click()
  await page.getByRole('textbox', { name: 'Text to test', exact: true }).fill('test_credential')
  await page.getByRole('button', { name: 'Test matching', exact: true }).click()
  await expect(page.getByRole('alert').filter({ hasText: 'Matching test failed.' })).toBeVisible()
  await expect(page.getByRole('status').filter({ hasText: 'No existing rule matched' })).toHaveCount(0)
  rejectTest = false
  await page.getByRole('button', { name: 'Test matching', exact: true }).click()
  await expect(page.getByRole('status').filter({ hasText: 'No existing rule matched' })).toBeVisible()
  await page.reload()
  await expect(toggle).toBeChecked()
  expect(saveAttempts).toBe(2)
})

test('credential protection separates setting load failure from empty rules and observations', async ({ page }) => {
  let settingsUnavailable = true
  await page.route('**/api/v1/settings/reversible_redaction_enabled', async (route) => {
    await route.fulfill(
      settingsUnavailable
        ? { status: 503, json: { error: 'Fixture settings unavailable' } }
        : { json: { data: 'false' } },
    )
  })
  await page.route('**/api/v1/reversible-redaction/**', async (route) => {
    const rules = new URL(route.request().url()).pathname.endsWith('/rules')
    await route.fulfill({
      json: {
        data: rules
          ? { rules: [], prefilter: '', filter: '' }
          : { items: [], next_cursor: null, observation_gap: false },
      },
    })
  })
  await page.goto('/reversible-redaction')
  await expect(
    page.getByRole('alert').filter({ hasText: 'Credential protection settings could not be loaded.' }),
  ).toBeVisible({ timeout: 15_000 })
  await expect(page.getByRole('switch')).toBeDisabled()
  await expect(page.getByText('No rules are bundled with this instance.', { exact: true })).toBeVisible()
  await page.getByRole('tab', { name: 'Hit records', exact: true }).click()
  await expect(
    page.getByText('No new credential discoveries in the available observations.', { exact: true }),
  ).toBeVisible()
  settingsUnavailable = false
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(page.getByRole('switch')).toBeEnabled()
  await expect(page.getByRole('switch')).not.toBeChecked()
})
