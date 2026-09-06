import { expect, test, type Page } from '@playwright/test'
import type { ApiKey, Route } from '../src/lib/types'
import { prepareApp } from './prepare-app'

const model: Route = {
  id: 'route-one',
  model_id: 'upstream-exact',
  display_name: 'My model',
  balance: 'traffic_equalization',
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
    concurrency_limit: null,
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
    Object.defineProperty(navigator, 'clipboard', {
      value: {
        writeText: async () => {
          throw new Error('denied')
        },
      },
    })
  })
  await page.goto('/connect')
  await page.getByRole('button', { name: 'Copy', exact: true }).click()
  await expect(page.getByText('Could not copy to clipboard')).toBeVisible()
  await expect(page.getByText('Copied to clipboard', { exact: true })).toHaveCount(0)
  await expect(page.locator('pre')).toBeVisible()
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
