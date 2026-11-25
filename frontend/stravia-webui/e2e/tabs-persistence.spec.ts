import { expect, test } from '@playwright/test'
import { prepareApp } from './prepare-app'

test.describe('multi-tab state persistence', () => {
  test('connect persists active tab across reload and restores code options on back navigation', async ({ page }) => {
    await prepareApp(page)
    await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [] } }))
    await page.route('**/api/v1/api-keys', (route) => route.fulfill({ json: { data: [] } }))
    await page.goto('/connect')

    await page.getByRole('tab', { name: 'Code', exact: true }).click()
    await expect(page).toHaveURL(/\/connect\?tab=code$/)
    await page.getByRole('tab', { name: 'cURL', exact: true }).click()

    await page.reload()
    await expect(page.getByRole('tab', { name: 'Code', exact: true })).toHaveAttribute('aria-selected', 'true')

    await page.getByRole('tab', { name: 'cURL', exact: true }).click()
    await page.getByRole('link', { name: 'Providers', exact: true }).click()
    await expect(page).toHaveURL(/\/providers$/)

    await page.goBack()
    await expect(page).toHaveURL(/\/connect\?tab=code$/)
    await expect(page.getByRole('tab', { name: 'Code', exact: true })).toHaveAttribute('aria-selected', 'true')
    await expect(page.getByRole('tab', { name: 'cURL', exact: true })).toHaveAttribute('aria-selected', 'true')
  })

  test('vendor plugins persists active tab and search query on back navigation', async ({ page }) => {
    await prepareApp(page)
    await page.route('**/api/v1/vendor-plugins', (route) => route.fulfill({ json: { data: [] } }))
    await page.route('**/api/v1/plugin-marketplace**', (route) =>
      route.fulfill({ json: { data: { sources: [], entries: [], update_count: 0 } } }),
    )
    await page.goto('/vendor-plugins')

    await page.getByRole('tab', { name: 'Installed', exact: true }).click()
    await expect(page).toHaveURL(/\/vendor-plugins\?tab=installed$/)
    const search = page.getByPlaceholder(/search plugins/i)
    await search.fill('openai')

    await page.getByRole('link', { name: 'Providers', exact: true }).click()
    await expect(page).toHaveURL(/\/providers$/)

    await page.goBack()
    await expect(page).toHaveURL(/\/vendor-plugins\?tab=installed$/)
    await expect(page.getByRole('tab', { name: 'Installed', exact: true })).toHaveAttribute('aria-selected', 'true')
    await expect(page.getByPlaceholder(/search plugins/i)).toHaveValue('openai')
  })

  test('reversible redaction persists rules tab/filter on back navigation and keeps records mounted', async ({
    page,
  }) => {
    await prepareApp(page)
    let discoveryRequests = 0
    await page.route('**/api/v1/settings/reversible_redaction_enabled', (route) =>
      route.fulfill({ json: { data: { key: 'reversible_redaction_enabled', value: 'true' } } }),
    )
    await page.route('**/api/v1/credential-protection/rules', (route) =>
      route.fulfill({ json: { data: { rules: [] } } }),
    )
    await page.route('**/api/v1/credential-protection/custom-rules', (route) => route.fulfill({ json: { data: [] } }))
    await page.route('**/api/v1/reversible-redaction/discoveries**', (route) => {
      discoveryRequests += 1
      return route.fulfill({ json: { data: { items: [], next_cursor: null } } })
    })

    await page.goto('/reversible-redaction')
    await expect.poll(() => discoveryRequests).toBe(1)

    await page.getByRole('tab', { name: /Current rules/ }).click()
    await expect(page).toHaveURL(/\/reversible-redaction\?tab=rules$/)
    await page.locator('#credential-rule-search').fill('github')

    // 切回 Hit records 再切回 Current rules：HitRecords 保持挂载，不重新拉取首屏列表。
    await page.getByRole('tab', { name: 'Hit records', exact: true }).click()
    await expect(page).toHaveURL(/\/reversible-redaction$/)
    expect(discoveryRequests).toBe(1)

    await page.getByRole('tab', { name: /Current rules/ }).click()
    await expect(page).toHaveURL(/\/reversible-redaction\?tab=rules$/)

    await page.getByRole('link', { name: 'Providers', exact: true }).click()
    await expect(page).toHaveURL(/\/providers$/)

    await page.goBack()
    await expect(page).toHaveURL(/\/reversible-redaction\?tab=rules$/)
    await expect(page.getByRole('tab', { name: /Current rules/ })).toHaveAttribute('aria-selected', 'true')
    await expect(page.locator('#credential-rule-search')).toHaveValue('github')
  })

  test('logs persists active tab across reload and back navigation', async ({ page }) => {
    await prepareApp(page)
    await page.route('**/api/v1/observations/forest**', (route) =>
      route.fulfill({ json: { data: { roots: [], total: 0, next_cursor: null, snapshot_sequence: 1 } } }),
    )
    await page.route('**/api/v1/observations/client-errors**', (route) =>
      route.fulfill({ json: { data: { items: [], total: 0, next_cursor: null, snapshot_sequence: 1 } } }),
    )
    await page.route('**/api/v1/observations/stream**', (route) =>
      route.fulfill({ status: 200, contentType: 'text/event-stream', body: '' }),
    )

    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Client Errors', exact: true }).click()
    await expect(page).toHaveURL(/\/logs\?tab=client-errors$/)

    await page.reload()
    await expect(page.getByRole('tab', { name: 'Client Errors', exact: true })).toHaveAttribute('aria-selected', 'true')

    await page.getByRole('link', { name: 'Providers', exact: true }).click()
    await expect(page).toHaveURL(/\/providers$/)

    await page.goBack()
    await expect(page).toHaveURL(/\/logs\?tab=client-errors$/)
    await expect(page.getByRole('tab', { name: 'Client Errors', exact: true })).toHaveAttribute('aria-selected', 'true')
  })
})
