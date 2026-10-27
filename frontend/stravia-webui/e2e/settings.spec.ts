import { expect, test, type Page } from '@playwright/test'

import type {
  CredentialDiscoveryPage,
  CredentialRuleCatalog,
  MediaUnderstandingConfigView,
  MediaUnderstandingKindConfig,
  UpdateMediaUnderstandingConfig,
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

const disabledAudio: MediaUnderstandingKindConfig = {
  enabled: false,
  model_id: null,
  thinking_level: null,
  state: 'disabled',
  eligible_models: [],
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test('file settings save switches immediately and keep an unsaved address draft across a failed save', async ({
  page,
}) => {
  let saved = {
    client_base_url: 'https://client.example:8443/prefix',
    external_signed_downloads: false,
    file_public_base_url: null as string | null,
    upload_prompt_injection: false,
    s3: null,
  }
  let rejectSave = false
  const writes: (typeof saved)[] = []
  await page.route('**/api/v1/settings/artifact_settings', async (route) => {
    if (route.request().method() === 'PUT') {
      if (rejectSave) return route.fulfill({ status: 503, json: { error: 'File settings unavailable' } })
      saved = JSON.parse(route.request().postDataJSON().value)
      writes.push(saved)
    }
    await route.fulfill({ json: { data: JSON.stringify(saved) } })
  })
  await page.goto('/settings?view=data')
  const section = page.locator('#artifacts')
  const client = page.locator('#artifact-client-base-url')
  const upload = page.locator('#artifact-upload-injection')

  // Typing alone writes nothing; the switch beside it saves on its own and leaves the draft untouched.
  await client.fill('https://edited.example:9443/deployment')
  expect(writes).toHaveLength(0)
  await upload.click()
  await expect(upload).toBeChecked()
  await expect.poll(() => writes.length).toBe(1)
  expect(saved.upload_prompt_injection).toBe(true)
  expect(saved.client_base_url).toBe('https://client.example:8443/prefix')
  await expect(client).toHaveValue('https://edited.example:9443/deployment')

  // A failed switch returns to the confirmed value and offers a retry; the address draft survives.
  rejectSave = true
  await upload.click()
  await expect(section.getByRole('alert').filter({ hasText: 'File settings unavailable' })).toBeVisible()
  await expect(upload).toBeChecked()
  expect(saved.upload_prompt_injection).toBe(true)
  await expect(client).toHaveValue('https://edited.example:9443/deployment')

  // The address saves only on Enter or Save, and keeps the draft when the save fails.
  await client.press('Enter')
  await expect(section.locator('p[role="alert"]')).toContainText('File settings unavailable')
  await expect(client).toHaveValue('https://edited.example:9443/deployment')
  rejectSave = false
  await section.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(client).toHaveValue('https://edited.example:9443/deployment')
  await expect.poll(() => saved.client_base_url).toBe('https://edited.example:9443/deployment')
  expect(saved.upload_prompt_injection).toBe(true)
  await page.reload()
  await expect(client).toHaveValue('https://edited.example:9443/deployment')
  await expect(upload).toBeChecked()
})

test('external signed downloads and S3 storage take effect only after their sheet is saved', async ({ page }) => {
  let saved = {
    client_base_url: 'https://client.example/prefix',
    external_signed_downloads: false,
    file_public_base_url: null as string | null,
    upload_prompt_injection: false,
    s3: null as null | Record<string, unknown>,
  }
  const writes: (typeof saved)[] = []
  await page.route('**/api/v1/settings/artifact_settings', async (route) => {
    if (route.request().method() === 'PUT') {
      saved = JSON.parse(route.request().postDataJSON().value)
      writes.push(saved)
    }
    await route.fulfill({ json: { data: JSON.stringify(saved) } })
  })
  await page.goto('/settings?view=data')
  const external = page.locator('#artifact-external-downloads')
  const sheet = page.getByRole('dialog')

  // Cancelling the sheet keeps the original value and writes nothing.
  await external.click()
  await expect(sheet.getByLabel('File public access address')).toHaveValue('https://client.example/prefix')
  await sheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  await expect(external).not.toBeChecked()
  expect(writes).toHaveLength(0)

  // Choosing S3 opens its sheet; the storage stays internal until the fields are saved.
  await page.locator('#artifact-storage').click()
  await page.getByRole('option', { name: 'External S3', exact: true }).click()
  await sheet.getByLabel('S3 endpoint').fill('https://objects.example/storage')
  await sheet.getByLabel('S3 region').fill('local-region')
  await sheet.getByLabel('S3 bucket').fill('private-files')
  await sheet.getByLabel('S3 access key ID').fill('fixture-access')
  await expect(sheet.locator('#artifact-s3-secret_access_key')).toHaveAttribute('type', 'password')
  await sheet.locator('#artifact-s3-secret_access_key').fill('fixture-secret')
  await sheet.getByLabel('Credentials expire at (optional)').fill('2031-05-06T07:08')
  expect(writes).toHaveLength(0)
  await sheet.getByRole('button', { name: 'Save file settings', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  expect(writes).toHaveLength(1)
  expect(saved.s3).toMatchObject({
    endpoint: 'https://objects.example/storage',
    bucket: 'private-files',
    access_key_id: 'fixture-access',
    secret_access_key: 'fixture-secret',
    credentials_expires_at: new Date('2031-05-06T07:08').getTime(),
  })
  expect(saved.external_signed_downloads).toBe(false)

  await page.reload()
  await expect(page.locator('#artifact-storage')).toHaveText('External S3')
  await expect(external).not.toBeChecked()
  await external.click()
  await expect(sheet.getByLabel('File public access address')).toHaveValue('https://objects.example/storage')
  await sheet.getByLabel('File public access address').fill('https://downloads.example/objects')
  await sheet.getByRole('button', { name: 'Enable external signed downloads', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  await expect(external).toBeChecked()
  expect(saved).toMatchObject({
    external_signed_downloads: true,
    file_public_base_url: 'https://downloads.example/objects',
  })
  await page.reload()
  await expect(external).toBeChecked()
})

for (const locale of ['en-US', 'zh-CN']) {
  test(`file settings recover loading failures without showing disabled defaults (${locale})`, async ({ page }) => {
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    let failed = true
    await page.route('**/api/v1/settings/artifact_settings', (route) =>
      route.fulfill(
        failed
          ? { status: 503, json: { error: 'File baseline unavailable' } }
          : {
              json: {
                data: JSON.stringify({
                  client_base_url: 'https://saved.example/path',
                  external_signed_downloads: false,
                  file_public_base_url: null,
                  upload_prompt_injection: true,
                  s3: null,
                }),
              },
            },
      ),
    )
    await page.goto('/settings?view=data')
    await expect(page.locator('#artifacts').getByRole('alert')).toBeVisible()
    await expect(page.locator('#artifact-upload-injection')).toHaveCount(0)
    failed = false
    await page.locator('#artifacts').getByRole('button').click()
    await expect(page.locator('#artifact-upload-injection')).toBeChecked()
    await expect(page.locator('#artifact-client-base-url')).toHaveValue('https://saved.example/path')
    await page.setViewportSize({ width: 320, height: 900 })
    await expect(page.locator('#artifact-client-base-url')).toBeVisible()
    await expect(page.locator('#artifact-client-base-url')).toBeInViewport()
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })
}

test('inline settings commit only on Enter or Save, revert on Escape, and keep the draft after an error', async ({
  page,
}) => {
  let saved = '7'
  let reject = false
  const writes: string[] = []
  await page.route('**/api/v1/settings/log_retention_days', async (route) => {
    if (route.request().method() === 'PUT') {
      const value = route.request().postDataJSON().value as string
      writes.push(value)
      if (reject) return route.fulfill({ status: 503, json: { error: 'Retention storage unavailable' } })
      saved = value
      return route.fulfill({ json: { data: null } })
    }
    await route.fulfill({ json: { data: saved } })
  })
  await page.goto('/settings?view=data')
  const retention = page.getByRole('spinbutton', { name: 'Retention period (days)', exact: true })
  const section = page.locator('#logs')
  const save = section.getByRole('button', { name: 'Save', exact: true })
  const cancel = section.getByRole('button', { name: 'Cancel', exact: true })
  await expect(retention).toHaveValue('7')
  await expect(save).toHaveCount(0)

  // Blur never commits: the draft stays, the row offers Save and Cancel, nothing is written.
  await retention.fill('30')
  await page.locator('h1').click()
  await expect(retention).toHaveValue('30')
  await expect(save).toBeVisible()
  await expect(cancel).toBeVisible()
  expect(writes).toEqual([])

  // Escape restores the saved value and removes the actions.
  await retention.press('Escape')
  await expect(retention).toHaveValue('7')
  await expect(save).toHaveCount(0)
  expect(writes).toEqual([])

  // A failed save keeps the draft and the actions and explains why.
  reject = true
  await retention.fill('30')
  await retention.press('Enter')
  await expect(section.getByRole('alert')).toContainText('Retention storage unavailable')
  await expect(retention).toHaveValue('30')
  await expect(save).toBeEnabled()
  expect(saved).toBe('7')

  // Enter saves, shows the signal, and the value becomes the new baseline.
  reject = false
  await retention.press('Enter')
  await expect(section.getByRole('status').filter({ hasText: 'Saved' })).toBeVisible()
  await expect(save).toHaveCount(0)
  expect(saved).toBe('30')
  expect(writes).toEqual(['30', '30'])
  await page.reload()
  await expect(retention).toHaveValue('30')

  // The Save button commits as well, and Cancel reverts.
  await retention.fill('45')
  await save.click()
  await expect.poll(() => saved).toBe('45')
  await retention.fill('60')
  await cancel.click()
  await expect(retention).toHaveValue('45')

  // A value outside 1–365 is rejected locally without a request.
  const before = writes.length
  await retention.fill('400')
  await retention.press('Enter')
  await expect(section.getByRole('alert')).toBeVisible()
  expect(writes).toHaveLength(before)
})

test('setup prefills its entry and preserves an edited complete client address after failure', async ({ page }) => {
  let completed = false
  let rejectSave = true
  await page.route('**/api/v1/auth/state', (route) =>
    route.fulfill({
      json: {
        mode: completed ? 'server' : 'setup',
        authenticated: false,
        setup_authorized: !completed,
        username: null,
      },
    }),
  )
  await page.route('**/api/v1/setup/test', (route) => route.fulfill({ status: 204 }))
  let submitted = ''
  await page.route('**/api/v1/setup/complete', async (route) => {
    submitted = route.request().postDataJSON().client_base_url
    if (rejectSave) return route.fulfill({ status: 503, json: { error: 'Setup storage unavailable' } })
    completed = true
    await route.fulfill({ json: { mode: 'server' } })
  })
  await page.goto('/setup')
  const address = page.locator('#setup-client-base-url')
  await expect(address).toHaveValue(new URL(page.url()).origin)
  await address.fill('https://entry.example:8443/my/stravia')
  await page.locator('#setup-username').fill('administrator')
  await page.locator('#setup-password').fill('a-secure-setup-password')
  await page.locator('#setup-confirm-password').fill('a-secure-setup-password')
  await page.getByRole('button', { name: 'Test connection', exact: true }).click()
  await page.locator('form button[type="submit"]').click()
  await expect(address).toHaveValue('https://entry.example:8443/my/stravia')
  await expect(page.locator('form').getByRole('alert')).toBeVisible()
  await expect(page.locator('form button[type="submit"]')).toBeEnabled()
  expect(completed).toBe(false)
  rejectSave = false
  await page.locator('form button[type="submit"]').click()
  await expect(page).toHaveURL(/\/login$/)
  expect(submitted).toBe('https://entry.example:8443/my/stravia')
})

const boxOf = (page: Page, selector: string) =>
  page.evaluate((selector) => {
    const element = document.querySelector(selector)
    if (!element) throw new Error(`Missing element: ${selector}`)
    const { x, y, width, height } = element.getBoundingClientRect()
    return { x, y, width, height, right: x + width, bottom: y + height }
  }, selector)

test('settings fields align in wide containers and stack in narrow containers', async ({ page }) => {
  await page.setViewportSize({ width: 1500, height: 900 })
  await page.goto('/settings')
  await expect(page.locator('#theme-preference')).toBeVisible()
  await page.evaluate(() => document.fonts.ready.then(() => undefined))

  const column = '.settings-column'
  const wideColumn = await boxOf(page, column)
  const wideLabel = await boxOf(page, 'label[for="theme-preference"]')
  const wideTheme = await boxOf(page, '#theme-preference')
  const wideLanguage = await boxOf(page, '#interface-language')
  expect(wideLabel.right).toBeLessThan(wideTheme.x)
  // Short selects keep their 16rem semantic width and sit on the settings column's right edge.
  expect(Math.abs(wideTheme.width - 256)).toBeLessThan(1)
  expect(Math.abs(wideLanguage.width - 256)).toBeLessThan(1)
  expect(Math.abs(wideTheme.right - wideColumn.right)).toBeLessThan(1)
  expect(Math.abs(wideLanguage.right - wideColumn.right)).toBeLessThan(1)
  // The row top-aligns its title with the control (the title is one line, the control 36px).
  expect(Math.abs(wideLabel.y - wideTheme.y)).toBeLessThan(12)

  await page.goto('/settings?view=data')
  await expect(page.locator('#log-retention')).toBeVisible()
  await page.evaluate(() => document.fonts.ready.then(() => undefined))
  const dataColumn = await boxOf(page, column)
  const retention = await boxOf(page, '#log-retention')
  const clientAddress = await boxOf(page, '#artifact-client-base-url')
  expect(Math.abs(retention.width - 112)).toBeLessThan(1)
  expect(Math.abs(retention.right - dataColumn.right)).toBeLessThan(1)
  // Free-text rows stack: the title sits above a control that fills the settings column.
  const clientAddressLabel = await boxOf(page, 'label[for="artifact-client-base-url"]')
  expect(clientAddressLabel.bottom).toBeLessThanOrEqual(clientAddress.y)
  expect(Math.abs(clientAddress.x - dataColumn.x)).toBeLessThan(1)
  expect(Math.abs(clientAddress.right - dataColumn.right)).toBeLessThan(1)

  await page.goto('/settings?view=network')
  await expect(page.locator('#proxy-bypass')).toBeVisible()
  const bypass = await boxOf(page, '#proxy-bypass')
  const networkColumn = await boxOf(page, column)
  expect(Math.abs(bypass.x - networkColumn.x)).toBeLessThan(1)
  expect(Math.abs(bypass.right - networkColumn.right)).toBeLessThan(1)

  await page.setViewportSize({ width: 500, height: 900 })
  await page.goto('/settings')
  await expect(page.locator('#theme-preference')).toBeVisible()
  const narrowLabel = await boxOf(page, 'label[for="theme-preference"]')
  const narrowTheme = await boxOf(page, '#theme-preference')
  const narrowLanguage = await boxOf(page, '#interface-language')
  expect(narrowLabel.bottom).toBeLessThanOrEqual(narrowTheme.y)
  expect(Math.abs(narrowTheme.width - narrowLanguage.width)).toBeLessThan(1)
  expect(narrowTheme.width).toBeGreaterThan(256)
  await page.goto('/settings?view=data')
  await expect(page.getByRole('spinbutton', { name: 'Retention period (days)', exact: true })).toBeVisible()
  const narrowRetention = await boxOf(page, '#log-retention')
  expect(Math.abs(narrowRetention.width - narrowTheme.width)).toBeLessThan(1)
})

test('settings column stays 48rem and centered with its navigation on ultra-wide screens', async ({ page }) => {
  await page.setViewportSize({ width: 2560, height: 1440 })
  const rem = 16
  const rows = [
    ['/settings', 'label[for="theme-preference"]', '#theme-preference', 'inline'],
    ['/settings?view=data', 'label[for="log-retention"]', '#log-retention', 'inline'],
    ['/settings?view=network', 'label[for="proxy-bypass"]', '#proxy-bypass', 'stacked'],
  ] as const
  const edges: number[] = []
  for (const [url, labelSelector, controlSelector, layout] of rows) {
    await page.goto(url)
    await expect(page.locator(controlSelector)).toBeVisible()
    await page.evaluate(() => document.fonts.ready.then(() => undefined))
    const label = await boxOf(page, labelSelector)
    const control = await boxOf(page, controlSelector)
    const column = await boxOf(page, '.settings-column')
    if (layout === 'inline') expect(control.x - label.right).toBeGreaterThan(0)
    else expect(control.y).toBeGreaterThanOrEqual(label.bottom)
    expect(control.right - label.x).toBeLessThanOrEqual(48 * rem + 1)
    expect(column.width).toBeLessThanOrEqual(48 * rem + 1)
    edges.push(control.right - column.x)
    // The navigation and the column form one group centered in the route container.
    const route = await boxOf(page, '.route-page')
    const nav = await boxOf(page, 'nav[aria-label="Settings sections"]')
    expect(Math.abs(nav.x - route.x - (route.right - column.right))).toBeLessThan(1)
    expect(Math.abs(nav.x - (await boxOf(page, '.route-page-header')).x)).toBeLessThan(1)
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth),
    ).toBe(true)
  }
  // Every control ends on the settings column's right edge, whatever its layout.
  expect(Math.max(...edges) - Math.min(...edges)).toBeLessThan(1)
})

test('settings navigation switches between vertical and horizontal at a 64rem route width', async ({ page }) => {
  const measure = async () =>
    page.evaluate(() => {
      const route = document.querySelector('.route-page')!.getBoundingClientRect().width
      const nav = document.querySelector('nav[aria-label="Settings sections"]')!
      const links = [...nav.querySelectorAll('a')].map((link) => {
        const { x, y, width, height } = link.getBoundingClientRect()
        const icon = link.querySelector('svg')
        return { x, y, width, height, iconVisible: icon ? getComputedStyle(icon).display !== 'none' : false }
      })
      const navBox = nav.getBoundingClientRect()
      const routeBox = document.querySelector('.route-page')!.getBoundingClientRect()
      const column = document.querySelector('.settings-column')!.getBoundingClientRect()
      const header = document.querySelector('.route-page-header')!.getBoundingClientRect()
      const firstTitle = document.querySelector('.settings-column h2')!.getBoundingClientRect()
      return {
        route,
        routeX: routeBox.x,
        nav: { x: navBox.x, y: navBox.y, width: navBox.width, height: navBox.height, scrollW: nav.scrollWidth },
        links,
        column: { x: column.x, width: column.width },
        headerX: header.x,
        firstTitleY: firstTitle.y,
        overflow: document.documentElement.scrollWidth <= innerWidth,
      }
    })

  const seen = new Set<string>()
  for (const width of [1100, 1280, 1310, 1320, 1328, 1336, 1360, 1440, 1920, 2560]) {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/settings')
    await expect(page.locator('#theme-preference')).toBeVisible()
    await page.evaluate(() => document.fonts.ready.then(() => undefined))
    const m = await measure()
    const vertical = m.route >= 1024
    seen.add(vertical ? 'vertical' : 'horizontal')
    expect(m.overflow).toBe(true)
    expect(m.links).toHaveLength(5)
    if (vertical) {
      // Same links, stacked at the left: 12rem wide, 32px gap to a 48rem column; the pair is centered.
      expect(Math.abs(m.nav.width - 192)).toBeLessThan(1)
      expect(Math.abs(m.nav.x - m.headerX)).toBeLessThan(1)
      expect(Math.abs(m.column.x - (m.nav.x + m.nav.width + 32))).toBeLessThan(1)
      expect(Math.abs(m.column.width - 48 * 16)).toBeLessThan(1)
      expect(Math.abs(m.nav.x - m.routeX - (m.routeX + m.route - (m.column.x + m.column.width)))).toBeLessThan(1)
      expect(m.links.every((link) => link.iconVisible)).toBe(true)
      for (let index = 1; index < m.links.length; index += 1) {
        expect(m.links[index].y).toBeGreaterThan(m.links[index - 1].y)
        expect(Math.abs(m.links[index].x - m.links[0].x)).toBeLessThan(1)
      }
      expect(Math.abs(m.nav.y - m.firstTitleY)).toBeLessThanOrEqual(12)
    } else {
      // Tabs under the header, text only, wrapping instead of scrolling.
      expect(Math.abs(m.nav.x - m.headerX)).toBeLessThan(1)
      expect(m.links.every((link) => !link.iconVisible)).toBe(true)
      expect(m.nav.scrollW).toBeLessThanOrEqual(Math.ceil(m.nav.width))
      expect(m.column.x).toBeLessThanOrEqual(m.nav.x + 1)
      expect(m.firstTitleY).toBeGreaterThan(m.nav.y + m.nav.height - 1)
    }
  }
  expect([...seen].sort()).toEqual(['horizontal', 'vertical'])
})

test('settings groups are links with history, current state, and a General fallback', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/settings')
  const nav = page.getByRole('navigation', { name: 'Settings sections' })
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Settings')
  await expect(nav.getByRole('link')).toHaveText(['General', 'Network', 'Data', 'Account', 'Advanced'])
  await expect(nav.getByRole('link', { name: 'General' })).toHaveAttribute('aria-current', 'page')
  await expect(page).toHaveTitle('General · Settings · Stravia')
  await expect(page.getByRole('heading', { level: 2 })).toHaveText(['Appearance', 'Updates'])

  await nav.getByRole('link', { name: 'Network' }).click()
  await expect(page).toHaveURL(/\/settings\?view=network$/)
  await expect(nav.getByRole('link', { name: 'Network' })).toHaveAttribute('aria-current', 'page')
  await expect(nav.getByRole('link', { name: 'General' })).not.toHaveAttribute('aria-current', 'page')
  await expect(page).toHaveTitle('Network · Settings · Stravia')
  await expect(page.getByRole('heading', { level: 2 })).toHaveText(['Proxy'])
  await nav.getByRole('link', { name: 'Advanced' }).click()
  await expect(page.getByRole('heading', { level: 2 })).toHaveText(['Upstream RPM queue', 'Diagnostics'])
  await page.goBack()
  await expect(page).toHaveURL(/view=network$/)
  await expect(page.getByRole('heading', { level: 2, name: 'Proxy' })).toBeVisible()

  // Unknown or differently-cased values show General without rewriting the address.
  for (const view of ['billing', 'Network', '']) {
    await page.goto(`/settings?view=${view}`)
    await expect(nav.getByRole('link', { name: 'General' })).toHaveAttribute('aria-current', 'page')
    await expect(page.getByRole('heading', { level: 2, name: 'Appearance' })).toBeVisible()
    await expect(page).toHaveURL(new RegExp(`/settings\\?view=${view}$`))
  }
})

test('the Account group is hidden and falls back to General outside Server hosts', async ({ page }) => {
  await page.route('**/api/v1/auth/state', (route) =>
    route.fulfill({ json: { mode: 'desktop', authenticated: true, setup_authorized: false, username: null } }),
  )
  await page.goto('/settings?view=account')
  const nav = page.getByRole('navigation', { name: 'Settings sections' })
  await expect(nav.getByRole('link')).toHaveText(['General', 'Network', 'Data', 'Advanced'])
  await expect(nav.getByRole('link', { name: 'General' })).toHaveAttribute('aria-current', 'page')
  await expect(page.getByRole('heading', { level: 2, name: 'Appearance' })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Administrator credentials' })).toHaveCount(0)
  await expect(page).toHaveURL(/\/settings\?view=account$/)
})

test('settings groups hide host-specific groups and rows instead of disabling them', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.goto('/settings?view=account')
  const nav = page.getByRole('navigation', { name: 'Settings sections' })
  await expect(nav.getByRole('link', { name: 'Account' })).toHaveAttribute('aria-current', 'page')
  await expect(page.getByRole('heading', { level: 2 })).toHaveText(['Administrator credentials'])
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Settings')

  // Server host: no Desktop-only sections anywhere.
  for (const view of ['general', 'network']) {
    await page.goto(`/settings?view=${view}`)
    await expect(page.getByRole('heading', { level: 2 }).first()).toBeVisible()
    await expect(page.getByRole('heading', { name: 'Startup', exact: true })).toHaveCount(0)
    await expect(page.getByRole('heading', { name: 'Client', exact: true })).toHaveCount(0)
    await expect(page.locator('#desktop-fixed-port, #desktop-launch-at-login')).toHaveCount(0)
  }
})

test('administrator credentials are edited in a sheet that states every session signs out', async ({ page }) => {
  let submitted: unknown
  await page.route('**/api/v1/auth/credentials', async (route) => {
    submitted = route.request().postDataJSON()
    await route.fulfill({ status: 204 })
  })
  await page.goto('/settings?view=account')
  await expect(page.getByText('playwright-admin', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: /^Edit/ }).click()
  const sheet = page.getByRole('dialog', { name: 'Administrator credentials' })
  await expect(sheet.getByText('Saving signs out every session.')).toBeVisible()
  const save = sheet.getByRole('button', { name: 'Save credentials', exact: true })
  await expect(save).toBeDisabled()
  // 无草稿时取消直接关闭，不弹放弃确认。
  await sheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  await expect(page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })).toHaveCount(0)

  await page.getByRole('button', { name: /^Edit/ }).click()
  await sheet.getByLabel('Current password').fill('old-password')
  await sheet.getByLabel('New password').fill('new-password')
  await sheet.getByLabel('Confirm password').fill('different')
  await expect(sheet.getByText('The passwords must match.')).toBeVisible()
  await expect(save).toBeDisabled()
  await sheet.getByLabel('Confirm password').fill('new-password')
  await sheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  const discard = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })
  await expect(discard).toBeVisible()
  expect(submitted).toBeUndefined()
  await discard.getByRole('button', { name: 'Discard changes', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  expect(submitted).toBeUndefined()

  await page.getByRole('button', { name: /^Edit/ }).click()
  await expect(sheet.getByLabel('Current password')).toHaveValue('')
  await sheet.getByLabel('Current password').fill('old-password')
  await sheet.getByLabel('New password').fill('new-password')
  await sheet.getByLabel('Confirm password').fill('new-password')
  await save.click()
  await expect
    .poll(() => submitted)
    .toEqual({ current_password: 'old-password', username: 'playwright-admin', password: 'new-password' })
})

test('advanced features keep separate media and web search surfaces', async ({ page }) => {
  let searchConfig = {
    revision: 3,
    enabled: true,
    backend: { kind: 'local', model_id: 'model-search' } as { kind: string; model_id?: string; route_id?: string },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-08-17T00:00:00Z',
    limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
  }
  const searchSaves: unknown[] = []
  const mediaConfig = {
    image: {
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
    },
    audio: {
      enabled: false,
      model_id: null,
      thinking_level: null,
      state: 'disabled',
      eligible_models: [
        {
          id: 'model-audio',
          model_id: 'listener-model',
          display_name: 'Listener model',
          supported_thinking_levels: ['medium'],
        },
      ],
    },
  } satisfies MediaUnderstandingConfigView

  await page.route('**/api/v1/web-search/config', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      searchSaves.push(input)
      searchConfig = { ...searchConfig, ...input, revision: searchConfig.revision + 1 }
    }
    await route.fulfill({ json: { data: searchConfig } })
  })
  await page.route('**/api/v1/web-search/eligible-models', async (route) => {
    await route.fulfill({
      json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] },
    })
  })
  await page.route('**/api/v1/web-search/external-routes', async (route) => {
    await route.fulfill({
      json: {
        data: [{ id: 'route-external', model_id: 'external-search', display_name: 'External search', available: true }],
      },
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

  await page.setViewportSize({ width: 2560, height: 1440 })
  await page.goto('/media-understanding')
  const navigation = page.getByRole('navigation', { name: 'Primary navigation' })
  await expect(navigation.getByText('Advanced Features', { exact: true })).toBeVisible()
  await expect(navigation.getByRole('link', { name: 'Media understanding' })).toBeVisible()
  await expect(navigation.getByRole('link', { name: 'Web search' })).toBeVisible()
  const title = page.getByRole('heading', { level: 1, name: 'Media understanding', exact: true })
  await expect(title).toBeVisible()
  const enable = page.getByRole('switch', { name: 'Enable image understanding' })
  await expect(enable).toBeChecked()
  const columnBox = (await enable.locator('xpath=ancestor::section[1]').boundingBox())!
  expect(columnBox.width).toBeLessThanOrEqual(48 * 16 + 1)
  // 页头与设置列同宽，一起在内容容器中居中。
  const routeBox = (await page.locator('.route-page').boundingBox())!
  expect(
    Math.abs(columnBox.x - routeBox.x - (routeBox.x + routeBox.width - columnBox.x - columnBox.width)),
  ).toBeLessThan(2)
  expect(Math.abs(columnBox.x - (await title.boundingBox())!.x)).toBeLessThan(2)
  expect(await page.locator('header').filter({ has: title }).getByRole('switch').count()).toBe(0)
  await expect(page.getByRole('button', { name: 'Save settings', exact: true })).toHaveCount(0)
  await expect(page.getByText('Multimodal model · high', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Edit Image understanding model' }).click()
  const mediaSheet = page.locator('[data-slot="sheet-content"]')
  await expect(mediaSheet.locator('#media-model-image')).toHaveText('Multimodal model')
  await mediaSheet.locator('#media-model-image').click()
  await expect(page.getByText('multimodal-model', { exact: true })).toBeVisible()
  await page.getByRole('option', { name: 'Multimodal model' }).click()
  await expect(mediaSheet.locator('#media-thinking-level-image')).toHaveText('high')
  await mediaSheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(mediaSheet).toHaveCount(0)
  // 音频理解独立于图片：尚未绑定模型时开关不可用。
  const audioEnable = page.getByRole('switch', { name: 'Enable audio understanding' })
  await expect(audioEnable).not.toBeChecked()
  await expect(audioEnable).toBeDisabled()
  await expect(page.getByText('Not configured', { exact: true })).toBeVisible()

  await navigation.getByRole('link', { name: 'Web search' }).click()
  await expect(page).toHaveURL(/\/web-search$/)
  await expect(page.getByRole('heading', { level: 1, name: 'Web search', exact: true })).toBeVisible()
  await expect(page.locator('#web-search-sources')).toBeVisible()
  await expect(page.getByRole('switch')).toHaveCount(1)
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Local search limits' })).toBeVisible()
  await expect(page.locator('#search-max-turns')).toHaveValue('6')
  await expect(page.locator('#search-total-seconds')).toHaveValue('180')
  // 预算行不属于绑定：绑定 Sheet 内的方式与目标只是草稿，保存前不请求也不改变页面。
  await page.getByRole('button', { name: 'Edit search method', exact: true }).click()
  const sheet = page.getByRole('dialog', { name: 'Edit search method' })
  await expect(sheet.locator('#search-local-model')).toHaveText('Search model')
  await sheet.locator('#search-backend').click()
  await page.getByRole('option', { name: 'External Route' }).click()
  await expect(sheet.getByText('Search Route', { exact: true })).toBeVisible()
  await expect(sheet.locator('#search-external-route')).toHaveText('Select a Search Route')
  await expect(sheet.getByRole('button', { name: 'Save search method', exact: true })).toBeDisabled()
  await sheet.locator('#search-external-route').click()
  await page.getByRole('option', { name: 'External search' }).click()
  await expect(sheet.locator('#search-external-route')).toHaveText('External search')
  // 切换方式不提交半个绑定，也不丢掉另一种方式的草稿。
  await sheet.locator('#search-backend').click()
  await page.getByRole('option', { name: 'Use a Stravia model' }).click()
  await expect(sheet.locator('#search-local-model')).toHaveText('Search model')
  await sheet.locator('#search-backend').click()
  await page.getByRole('option', { name: 'External Route' }).click()
  await expect(sheet.locator('#search-external-route')).toHaveText('External search')
  expect(searchSaves).toHaveLength(0)
  await expect(page.locator('#web-search-sources')).toBeVisible()
  await sheet.getByRole('button', { name: 'Save search method', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  expect(searchSaves).toEqual([
    {
      revision: 3,
      enabled: true,
      backend: { kind: 'external', route_id: 'external-search' },
      max_turns: 6,
      total_time_seconds: 180,
      updated_at: '2026-08-17T00:00:00Z',
    },
  ])
  await expect(page.locator('#web-search-sources')).toHaveCount(0)
  await expect(page.getByRole('heading', { name: 'Local search limits' })).toHaveCount(0)
  await expect(page.getByText('External search', { exact: true })).toBeVisible()

  // 切回 Local：原预算仍在，Local 来源重新出现。
  await page.getByRole('button', { name: 'Edit search method', exact: true }).click()
  await sheet.locator('#search-backend').click()
  await page.getByRole('option', { name: 'Use a Stravia model' }).click()
  await expect(sheet.locator('#search-local-model')).toHaveText('Select a model')
  await sheet.locator('#search-local-model').click()
  await expect(page.getByText('search-model', { exact: true })).toBeVisible()
  await page.getByRole('option', { name: 'Search model' }).click()
  await sheet.getByRole('button', { name: 'Save search method', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  await expect(page.locator('#web-search-sources')).toBeVisible()
  expect(searchSaves[1]).toMatchObject({
    revision: 4,
    backend: { kind: 'local', model_id: 'model-search' },
    max_turns: 6,
    total_time_seconds: 180,
  })
  await expect(page.locator('#search-max-turns')).toHaveValue('6')
})

test('media settings can repair an unavailable binding and preserve the draft after a failed save', async ({
  page,
}) => {
  let rejectSave = true
  let config: MediaUnderstandingKindConfig = {
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
  await page.route('**/api/v1/media-understanding**', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      submitted.push(input)
      if (rejectSave) {
        await route.fulfill({ status: 503, json: { error: 'Fixture media save unavailable' } })
        return
      }
      config = { ...config, ...input, state: 'available' }
    }
    await route.fulfill({ json: { data: { image: config, audio: disabledAudio } } })
  })
  await page.goto('/media-understanding')
  const unavailable = page.getByRole('status').filter({ hasText: 'Saved media settings are unavailable' })
  await expect(unavailable).toBeVisible()
  await expect(page.getByText('Fixture vision · high', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Edit Image understanding model' }).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await expect(sheet.locator('#media-model-image')).toHaveText('Fixture vision')
  await expect(sheet.locator('#media-thinking-level-image')).toHaveText('medium')
  const save = sheet.getByRole('button', { name: 'Save image understanding model', exact: true })
  await expect(save).toBeEnabled()
  await save.click()
  await expect(sheet.getByRole('alert').filter({ hasText: 'Fixture media save unavailable' })).toBeVisible()
  await expect(sheet.locator('#media-model-image')).toHaveText('Fixture vision')
  await expect(sheet.locator('#media-thinking-level-image')).toHaveText('medium')
  await expect(unavailable).toBeVisible()
  rejectSave = false
  await save.click()
  await expect(sheet).toHaveCount(0)
  await expect(unavailable).toHaveCount(0)
  await expect(page.getByText('Fixture vision · medium', { exact: true })).toBeVisible()
  expect(submitted).toEqual([
    { enabled: true, model_id: 'media-fixture', thinking_level: 'medium' },
    { enabled: true, model_id: 'media-fixture', thinking_level: 'medium' },
  ])
})

test('media activation saves only confirmed configuration without discarding the model draft', async ({ page }) => {
  let config: MediaUnderstandingKindConfig = {
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
  await page.route('**/api/v1/media-understanding**', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      submitted.push(input)
      config = { ...config, ...input }
    }
    await route.fulfill({ json: { data: { image: config, audio: disabledAudio } } })
  })
  await page.goto('/media-understanding')
  await page.getByRole('button', { name: 'Edit Image understanding model' }).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await sheet.locator('#media-thinking-level-image').click()
  await page.getByRole('option', { name: 'medium', exact: true }).click()
  // 有未保存的绑定草稿时，Escape 先确认；保留编辑后草稿仍在。
  const discard = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })
  await page.keyboard.press('Escape')
  await expect(discard).toBeVisible()
  await discard.getByRole('button', { name: 'Keep editing' }).click()
  await expect(discard).toHaveCount(0)
  await expect(sheet.locator('#media-thinking-level-image')).toHaveText('medium')
  await sheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await discard.getByRole('button', { name: 'Discard changes' }).click()
  await expect(sheet).toHaveCount(0)
  expect(submitted).toEqual([])
  await expect(page.getByText('Fixture vision · high', { exact: true })).toBeVisible()

  await page.getByRole('button', { name: 'Edit Image understanding model' }).click()
  await sheet.locator('#media-thinking-level-image').click()
  await page.getByRole('option', { name: 'medium', exact: true }).click()
  expect(submitted).toEqual([])
  await sheet.getByRole('button', { name: 'Save image understanding model', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  expect(submitted).toEqual([{ enabled: true, model_id: 'media-fixture', thinking_level: 'medium' }])
  await expect(page.getByText('Fixture vision · medium', { exact: true })).toBeVisible()

  // 独立启停只提交服务器确认的配置。
  const toggle = page.getByRole('switch', { name: 'Enable image understanding' })
  await toggle.click()
  await expect(toggle).not.toBeChecked()
  await expect(toggle).toBeEnabled()
  expect(submitted.at(-1)).toEqual({ enabled: false, model_id: 'media-fixture', thinking_level: 'medium' })
  await page.reload()
  await expect(toggle).not.toBeChecked()
  await expect(page.getByText('Fixture vision · medium', { exact: true })).toBeVisible()
})

test('audio understanding saves to its own endpoint without touching the image binding', async ({ page }) => {
  const image: MediaUnderstandingKindConfig = {
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
  let audio: MediaUnderstandingKindConfig = {
    enabled: false,
    model_id: null,
    thinking_level: null,
    state: 'disabled',
    eligible_models: [
      {
        id: 'audio-fixture',
        model_id: 'listener-fixture',
        display_name: 'Listener model',
        supported_thinking_levels: ['medium', 'high'],
      },
    ],
  }
  const submitted: { path: string; body: UpdateMediaUnderstandingConfig }[] = []
  await page.route('**/api/v1/media-understanding**', async (route) => {
    if (route.request().method() === 'PUT') {
      const body = route.request().postDataJSON() as UpdateMediaUnderstandingConfig
      submitted.push({ path: new URL(route.request().url()).pathname, body })
      audio = { ...audio, ...body, state: body.model_id ? 'available' : 'disabled' }
    }
    await route.fulfill({ json: { data: { image, audio } } })
  })
  await page.goto('/media-understanding')
  const audioToggle = page.getByRole('switch', { name: 'Enable audio understanding' })
  await expect(audioToggle).toBeDisabled()
  await page.getByRole('button', { name: 'Edit Audio understanding model' }).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await sheet.locator('#media-model-audio').click()
  await page.getByRole('option', { name: 'Listener model' }).click()
  await expect(sheet.locator('#media-thinking-level-audio')).toHaveText('medium')
  await sheet.getByRole('button', { name: 'Save audio understanding model', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  expect(submitted).toEqual([
    {
      path: '/api/v1/media-understanding/audio',
      body: { enabled: false, model_id: 'audio-fixture', thinking_level: 'medium' },
    },
  ])
  await expect(page.getByText('Listener model · medium', { exact: true })).toBeVisible()
  await expect(audioToggle).toBeEnabled()
  await audioToggle.click()
  await expect(audioToggle).toBeChecked()
  expect(submitted.at(-1)).toEqual({
    path: '/api/v1/media-understanding/audio',
    body: { enabled: true, model_id: 'audio-fixture', thinking_level: 'medium' },
  })
  await expect(page.getByText('Fixture vision · high', { exact: true })).toBeVisible()
  await expect(page.getByRole('switch', { name: 'Enable image understanding' })).toBeChecked()
})

test('an ineligible saved understanding binding stays visible and clearable at 390 and 320', async ({ page }) => {
  const eligible = [
    {
      id: 'media-fixture',
      model_id: 'vision-fixture',
      display_name: 'Fixture vision',
      supported_thinking_levels: ['medium'] as const,
    },
  ]
  let config: MediaUnderstandingKindConfig = {
    enabled: false,
    model_id: 'removed-model',
    thinking_level: 'high',
    state: 'unavailable',
    eligible_models: [...eligible],
  }
  const submitted: unknown[] = []
  await page.route('**/api/v1/media-understanding**', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON() as UpdateMediaUnderstandingConfig
      submitted.push(input)
      config = { ...config, ...input, state: input.model_id ? 'available' : 'unavailable' }
    }
    await route.fulfill({ json: { data: { image: config, audio: disabledAudio } } })
  })
  for (const width of [390, 320]) {
    config = {
      enabled: false,
      model_id: 'removed-model',
      thinking_level: 'high',
      state: 'unavailable',
      eligible_models: [...eligible],
    }
    await page.setViewportSize({ width, height: 844 })
    await page.goto('/media-understanding')
    // 已保存但不再合格的绑定仍显示原始技术 ID，而不是「未配置」。
    await expect(page.getByText('removed-model · high', { exact: true })).toBeVisible()
    await expect(page.getByRole('status').filter({ hasText: 'Saved media settings are unavailable' })).toBeVisible()
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)

    await page.getByRole('button', { name: 'Edit Image understanding model' }).click()
    const sheet = page.locator('[data-slot="sheet-content"]')
    await expect(sheet.locator('#media-model-image')).toHaveText('Select a model')
    const save = sheet.getByRole('button', { name: 'Save image understanding model', exact: true })
    // 功能关闭时允许保存空选择，清除失效绑定。
    await expect(save).toBeEnabled()
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
    await save.click()
    await expect(sheet).toHaveCount(0)
    expect(submitted.at(-1)).toEqual({ enabled: false, model_id: null, thinking_level: null })
    await expect(page.getByText('Not configured', { exact: true })).toBeVisible()
  }
  expect(submitted).toHaveLength(2)
})

test('media understanding Sheet locks submit, cancel and close while saving', async ({ page }) => {
  let release!: () => void
  const gate = new Promise<void>((resolve) => {
    release = resolve
  })
  let config: MediaUnderstandingKindConfig = {
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
  await page.route('**/api/v1/media-understanding**', async (route) => {
    if (route.request().method() === 'PUT') {
      await gate
      config = { ...config, ...(route.request().postDataJSON() as UpdateMediaUnderstandingConfig) }
    }
    await route.fulfill({ json: { data: { image: config, audio: disabledAudio } } })
  })
  await page.goto('/media-understanding')
  await page.getByRole('button', { name: 'Edit Image understanding model' }).click()
  const sheet = page.locator('[data-slot="sheet-content"]')
  await sheet.locator('#media-thinking-level-image').click()
  await page.getByRole('option', { name: 'medium', exact: true }).click()
  const save = sheet.getByRole('button', { name: /Save image understanding model/ })
  await save.click()
  await expect(save).toBeDisabled()
  await expect(save).toHaveAttribute('aria-busy', 'true')
  await expect(sheet.getByRole('button', { name: 'Cancel', exact: true })).toBeDisabled()
  await expect(sheet.getByRole('button', { name: 'Close', exact: true })).toBeDisabled()
  await page.keyboard.press('Escape')
  await expect(sheet).toBeVisible()
  release()
  await expect(sheet).toHaveCount(0)
  await expect(page.getByText('Fixture vision · medium', { exact: true })).toBeVisible()
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
  const seconds = page.locator('#search-total-seconds')
  await turns.fill('9')
  await seconds.fill('240')
  const turnsRow = page.locator('[data-slot=setting-row]').filter({ has: turns })
  await expect(turnsRow.getByRole('button', { name: 'Save', exact: true })).toBeVisible()
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
  await expect(seconds).toHaveValue('240')
  await expect(turnsRow.getByRole('button', { name: 'Save', exact: true })).toBeVisible()
  // 行内保存只携带本行：另一行的草稿不被提交，也不被清除。
  await turns.press('Enter')
  await expect(turnsRow.getByText('Saved', { exact: true })).toBeVisible()
  expect(submitted).toHaveLength(2)
  expect(submitted[1]).toMatchObject({ revision: 4, enabled: false, max_turns: 9, total_time_seconds: 180 })
  await expect(seconds).toHaveValue('240')
  await page.reload()
  await expect(toggle).not.toBeChecked()
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  await expect(turns).toHaveValue('9')
  await expect(seconds).toHaveValue('180')
})

test('unloaded settings stay non-editable until a failed baseline is recovered', async ({ page }) => {
  let unavailable = true
  await page.route('**/api/v1/settings/outbound_proxy', async (route) => {
    await route.fulfill(
      unavailable
        ? { status: 503, json: { error: 'Fixture settings unavailable' } }
        : { json: { data: JSON.stringify({ url: 'http://127.0.0.1:7890', bypass: '', force_http1: true }) } },
    )
  })
  await page.goto('/settings?view=network')
  await expect(page.locator('#proxy').getByRole('alert')).toBeVisible()
  await expect(page.locator('#proxy-bypass')).toHaveCount(0)
  await expect(page.getByRole('button', { name: /^Edit/ })).toHaveCount(0)
  unavailable = false
  await page.locator('#proxy').getByRole('button').click()
  await expect(page.locator('#proxy')).toContainText('http://127.0.0.1:7890')
  await page.getByRole('button', { name: /^Edit/ }).click()
  await expect(page.locator('#proxy-url')).toHaveValue('http://127.0.0.1:7890')
})

test('proxy URL saves one complete snapshot from its sheet and retains the draft after failure', async ({ page }) => {
  let saved = { url: 'http://127.0.0.1:7890', bypass: 'localhost', force_http1: true }
  const submitted: (typeof saved)[] = []
  let rejectSave = true
  let releaseSave: (() => void) | undefined
  await page.route('**/api/v1/settings/outbound_proxy', async (route) => {
    if (route.request().method() === 'PUT') {
      const value = JSON.parse(route.request().postDataJSON().value) as typeof saved
      submitted.push(value)
      await new Promise<void>((resolve) => {
        releaseSave = resolve
      })
      if (rejectSave) return route.fulfill({ status: 503, json: { error: 'Proxy storage unavailable' } })
      saved = value
    }
    await route.fulfill({ json: { data: JSON.stringify(saved) } })
  })
  await page.goto('/settings?view=network')
  const sheet = page.getByRole('dialog', { name: 'Edit proxy' })
  const url = page.locator('#proxy-url')
  await page.getByRole('button', { name: /^Edit/ }).click()
  await url.fill('socks5h://127.0.0.1:1080')
  expect(submitted).toHaveLength(0)
  await sheet.getByRole('button', { name: 'Save proxy', exact: true }).click()
  await expect(url).toBeDisabled()
  await expect.poll(() => submitted.length).toBe(1)
  expect(submitted).toEqual([{ url: 'socks5h://127.0.0.1:1080', bypass: 'localhost', force_http1: true }])
  releaseSave?.()
  await expect(sheet.getByRole('alert')).toContainText('Proxy storage unavailable')
  await expect(url).toHaveValue('socks5h://127.0.0.1:1080')
  expect(saved.url).toBe('http://127.0.0.1:7890')
  rejectSave = false
  await sheet.getByRole('button', { name: 'Save proxy', exact: true }).click()
  await expect.poll(() => submitted.length).toBe(2)
  releaseSave?.()
  await expect(sheet).toHaveCount(0)
  await page.reload()
  await expect(page.locator('#proxy')).toContainText('socks5h://127.0.0.1:1080')
  expect(submitted).toHaveLength(2)
})

test('bypass hosts save inline with the saved proxy URL and keep the draft after failure', async ({ page }) => {
  let saved = { url: 'http://127.0.0.1:7890', bypass: 'localhost', force_http1: true }
  const submitted: (typeof saved)[] = []
  let rejectSave = true
  await page.route('**/api/v1/settings/outbound_proxy', async (route) => {
    if (route.request().method() === 'PUT') {
      const value = JSON.parse(route.request().postDataJSON().value) as typeof saved
      submitted.push(value)
      if (rejectSave) return route.fulfill({ status: 503, json: { error: 'Proxy storage unavailable' } })
      saved = value
    }
    await route.fulfill({ json: { data: JSON.stringify(saved) } })
  })
  await page.goto('/settings?view=network')
  const bypass = page.locator('#proxy-bypass')
  await bypass.fill('localhost,.internal')
  expect(submitted).toHaveLength(0)
  await bypass.press('Enter')
  await expect(page.locator('#proxy').getByRole('alert')).toContainText('Proxy storage unavailable')
  await expect(bypass).toHaveValue('localhost,.internal')
  expect(submitted).toEqual([{ url: 'http://127.0.0.1:7890', bypass: 'localhost,.internal', force_http1: true }])
  rejectSave = false
  await bypass.press('Enter')
  await expect(page.locator('#proxy').getByRole('status')).toHaveText('Saved')
  await page.reload()
  await expect(bypass).toHaveValue('localhost,.internal')
  expect(submitted).toHaveLength(2)
})

test('update proxy preference uses saved configuration without submitting the proxy draft', async ({ page }) => {
  let savedProxy = { url: '', bypass: 'localhost', force_http1: true }
  let preference = 'false'
  let configWrites = 0
  const preferences: string[] = []
  let rejectPreference = false
  await page.route('**/api/v1/settings/outbound_proxy', async (route) => {
    if (route.request().method() === 'PUT') {
      configWrites++
      savedProxy = JSON.parse(route.request().postDataJSON().value)
    }
    await route.fulfill({ json: { data: JSON.stringify(savedProxy) } })
  })
  await page.route('**/api/v1/settings/update_use_proxy', async (route) => {
    if (route.request().method() === 'PUT') {
      preferences.push(route.request().postDataJSON().value)
      if (rejectPreference) return route.fulfill({ status: 503, json: { error: 'Preference unavailable' } })
      preference = route.request().postDataJSON().value
    }
    await route.fulfill({ json: { data: preference } })
  })
  await page.goto('/settings')
  const toggle = page.locator('#update-use-proxy')
  await toggle.click()
  const configure = page.locator('#updates a[href$="view=network#proxy"]')
  await expect(configure).toBeVisible()
  await expect(toggle).not.toBeChecked()
  expect(preferences).toEqual([])
  expect(configWrites).toBe(0)
  savedProxy = { ...savedProxy, url: 'http://127.0.0.1:7890' }
  rejectPreference = true
  await toggle.click()
  await expect(page.locator('#updates').getByRole('alert')).toBeVisible()
  await expect(toggle).not.toBeChecked()
  rejectPreference = false
  await page.locator('#updates').getByRole('alert').getByRole('button').click()
  await expect(toggle).toBeChecked()
  await toggle.click()
  await expect(toggle).not.toBeChecked()
  expect(preferences).toEqual(['true', 'true', 'false'])
  expect(configWrites).toBe(0)
})

test('update proxy preference recovers loading failures without displaying an unchecked default', async ({ page }) => {
  let unavailable = true
  await page.route('**/api/v1/settings/update_use_proxy', (route) =>
    route.fulfill(
      unavailable ? { status: 503, json: { error: 'Preference unavailable' } } : { json: { data: 'true' } },
    ),
  )
  await page.goto('/settings')
  await expect(page.locator('#updates').getByRole('alert')).toBeVisible()
  await expect(page.locator('#update-use-proxy')).toHaveCount(0)
  unavailable = false
  await page.locator('#updates').getByRole('alert').getByRole('button').click()
  await expect(page.locator('#update-use-proxy')).toBeChecked()
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

test('External Route search activation does not depend on Local sources', async ({ page }) => {
  let config = {
    revision: 1,
    enabled: false,
    backend: { kind: 'external', route_id: 'external-search' },
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
  await page.route('**/api/v1/web-search/external-routes', (route) =>
    route.fulfill({
      json: {
        data: [{ id: 'route-external', model_id: 'external-search', display_name: 'External search', available: true }],
      },
    }),
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

test('source load recovery requires a configured browser before Local sources can enable search', async ({ page }) => {
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
  browserPath = null
  await page.reload()
  await expect(searchCheckbox).toBeChecked()
  await expect(searchCheckbox).toBeEnabled()
  await searchCheckbox.click()
  await expect(searchCheckbox).not.toBeChecked()
  await expect(fetchCheckbox).toBeEnabled()
  await fetchCheckbox.click()
  await expect(fetchCheckbox).not.toBeChecked()
})

test('remote sources remain usable while a selected Local browser is unavailable', async ({ page }) => {
  const localSource: WebProvider = { ...searchSource, id: 'source-local', name: 'Local source', kind: 'local' }
  let saved: WebAccessSettings = {
    search_provider_ids: [localSource.id, searchSource.id],
    fetch_provider_ids: [localSource.id, searchSource.id],
  }
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
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [localSource, searchSource] } }))
  await page.route('**/api/v1/web-access/settings', async (route) => {
    if (route.request().method() === 'PUT') saved = route.request().postDataJSON()
    await route.fulfill({ json: { data: saved } })
  })
  await page.route('**/api/v1/web-access/browser', (route) =>
    route.fulfill({ status: 503, json: { error: 'Fixture browser load unavailable' } }),
  )
  await page.goto('/web-search')
  const searchSwitch = page.getByRole('switch', { name: 'Enable web search' })
  await expect(page.locator('#web-search-sources')).toContainText('Browser configuration could not be loaded')
  await expect(searchSwitch).toBeEnabled()
  await searchSwitch.click()
  await expect(searchSwitch).toBeChecked()
  const localCheckbox = page.locator('#web-access-search-source-local')
  await expect(localCheckbox).toBeEnabled()
  await localCheckbox.click()
  await expect(localCheckbox).not.toBeChecked()
  expect(saved.search_provider_ids).toEqual([searchSource.id])
})

for (const locale of ['en-US', 'zh-CN']) {
  test(`Local browser editor retains drafts, reports partial saves, and resets to automatic (${locale})`, async ({
    page,
  }) => {
    const zh = locale === 'zh-CN'
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    const localSource: WebProvider = { ...searchSource, id: 'source-local', name: 'Local source', kind: 'local' }
    let browserLoadFails = true
    let browserSaveFails = true
    let serviceSaveFails = true
    let browserPath: string | null = null
    const browserSaves: Array<string | null> = []
    let serviceSaves = 0
    await page.route('**/api/v1/web-search/config', (route) =>
      route.fulfill({
        json: {
          data: {
            revision: 1,
            enabled: false,
            backend: { kind: 'local', model_id: 'model-search' },
            max_turns: 6,
            total_time_seconds: 180,
            updated_at: '2026-09-01T00:00:00Z',
            limits: { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 },
          },
        },
      }),
    )
    await page.route('**/api/v1/web-search/eligible-models', (route) =>
      route.fulfill({
        json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] },
      }),
    )
    await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [localSource] } }))
    await page.route('**/api/v1/web-providers/source-local', async (route) => {
      if (route.request().method() === 'PUT') {
        serviceSaves++
        if (serviceSaveFails) {
          await route.fulfill({ status: 503, json: { error: 'Fixture service save unavailable' } })
          return
        }
      }
      await route.fulfill({ json: { data: localSource } })
    })
    await page.route('**/api/v1/web-access/settings', (route) =>
      route.fulfill({ json: { data: { search_provider_ids: [], fetch_provider_ids: [] } } }),
    )
    await page.route('**/api/v1/web-access/browser', async (route) => {
      if (route.request().method() === 'GET' && browserLoadFails) {
        await route.fulfill({ status: 503, json: { error: 'Fixture browser load unavailable' } })
        return
      }
      if (route.request().method() === 'PUT') {
        if (browserSaveFails) {
          await route.fulfill({ status: 400, json: { error: 'Fixture invalid browser path' } })
          return
        }
        browserPath = route.request().postDataJSON().path
        browserSaves.push(browserPath)
      }
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
    await page.goto('/web-search')
    const sources = page.locator('#web-search-sources')
    const searchCheckbox = page.locator('#web-access-search-source-local')
    await expect(searchCheckbox).toBeDisabled()
    await expect(sources).toContainText(zh ? '无法加载浏览器配置' : 'Browser configuration could not be loaded')
    browserLoadFails = false
    await sources.getByRole('button', { name: zh ? '重试' : 'Retry', exact: true }).click()
    await expect(sources).toContainText(zh ? '请先在 Local 服务编辑窗口' : 'Configure a Chrome or Chromium executable')
    await sources.getByRole('button', { name: zh ? '编辑' : 'Edit', exact: true }).click()
    const dialog = page.getByRole('dialog')
    const path = dialog.locator('#web-provider-browser-path')
    const save = dialog.getByRole('button', { name: zh ? '保存服务' : 'Save service', exact: true })
    await path.fill('C:\\Browser\\chrome.exe')
    await save.click()
    await expect(dialog.getByRole('alert')).toContainText('Fixture invalid browser path')
    await expect(path).toHaveValue('C:\\Browser\\chrome.exe')
    expect(serviceSaves).toBe(0)
    browserSaveFails = false
    await save.click()
    await expect(dialog.getByRole('alert')).toContainText(
      zh
        ? '浏览器路径已保存，但服务设置保存失败'
        : 'The browser path was saved, but the service settings could not be saved',
    )
    await expect(path).toHaveValue('C:\\Browser\\chrome.exe')
    expect(browserSaves).toEqual(['C:\\Browser\\chrome.exe'])
    expect(serviceSaves).toBe(1)
    serviceSaveFails = false
    await save.click()
    await expect(dialog).toHaveCount(0)
    expect(browserSaves).toEqual(['C:\\Browser\\chrome.exe'])
    await expect(searchCheckbox).toBeEnabled()
    await page.reload()
    await expect(searchCheckbox).toBeEnabled()
    await sources.getByRole('button', { name: zh ? '编辑' : 'Edit', exact: true }).click()
    await expect(path).toHaveValue('C:\\Browser\\chrome.exe')
    await path.fill('')
    await save.click()
    await expect(dialog).toHaveCount(0)
    expect(browserSaves).toEqual(['C:\\Browser\\chrome.exe', null])
    await expect(searchCheckbox).toBeDisabled()
  })
}

const searchLimits = { min_turns: 1, max_turns: 20, min_total_time_seconds: 30, max_total_time_seconds: 900 }

test('binding and budget saves recover from revision conflicts without losing input', async ({ page }) => {
  let server = {
    revision: 3,
    enabled: false,
    backend: { kind: 'local', model_id: 'model-search' },
    max_turns: 6,
    total_time_seconds: 180,
    updated_at: '2026-09-01T00:00:00Z',
  }
  const accepted: Array<Record<string, unknown>> = []
  await page.route('**/api/v1/web-search/config', async (route) => {
    if (route.request().method() === 'PUT') {
      const input = route.request().postDataJSON()
      if (input.revision !== server.revision) {
        await route.fulfill({ status: 409, json: { error: 'Fixture revision conflict' } })
        return
      }
      accepted.push(input)
      server = { ...server, ...input, revision: server.revision + 1 }
    }
    await route.fulfill({ json: { data: { ...server, limits: searchLimits } } })
  })
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({
      json: {
        data: [
          { id: 'model-search', model_id: 'search-model', display_name: 'Search model' },
          { id: 'model-second', model_id: 'second-model', display_name: 'Second model' },
        ],
      },
    }),
  )
  await page.route('**/api/v1/web-access/settings', (route) =>
    route.fulfill({ json: { data: { search_provider_ids: [], fetch_provider_ids: [] } } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [] } }))
  await page.goto('/web-search')
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  const turns = page.locator('#search-max-turns')
  const seconds = page.locator('#search-total-seconds')
  const sheet = page.getByRole('dialog', { name: 'Edit search method' })

  // 绑定：另一处已提交新修订，旧快照被拒绝；Sheet 与选择原样保留，确认值刷新后重试成功。
  await page.getByRole('button', { name: 'Edit search method', exact: true }).click()
  await sheet.locator('#search-local-model').click()
  await page.getByRole('option', { name: 'Second model' }).click()
  server = { ...server, revision: 4, max_turns: 7 }
  await sheet.getByRole('button', { name: 'Save search method', exact: true }).click()
  await expect(sheet.getByRole('alert')).toContainText('Fixture revision conflict')
  await expect(sheet.locator('#search-local-model')).toHaveText('Second model')
  expect(accepted).toHaveLength(0)
  await expect(turns).toHaveValue('7')
  await sheet.getByRole('button', { name: 'Save search method', exact: true }).click()
  await expect(sheet).toHaveCount(0)
  expect(accepted).toEqual([
    {
      revision: 4,
      enabled: false,
      backend: { kind: 'local', model_id: 'model-second' },
      max_turns: 7,
      total_time_seconds: 180,
      updated_at: '2026-09-01T00:00:00Z',
    },
  ])

  // 预算：同样保留输入与错误，重试只覆盖本行。
  await seconds.fill('300')
  server = { ...server, revision: server.revision + 1, total_time_seconds: 200 }
  await seconds.press('Enter')
  const row = page.locator('[data-slot=setting-row]').filter({ has: seconds })
  await expect(row.getByRole('alert')).toContainText('Fixture revision conflict')
  await expect(seconds).toHaveValue('300')
  await expect(row.getByRole('button', { name: 'Save', exact: true })).toBeVisible()
  await expect.poll(() => accepted.length).toBe(1)
  await seconds.press('Enter')
  await expect(row.getByText('Saved', { exact: true })).toBeVisible()
  expect(accepted[1]).toMatchObject({
    revision: 6,
    backend: { kind: 'local', model_id: 'model-second' },
    max_turns: 7,
    total_time_seconds: 300,
  })
  await expect(turns).toHaveValue('7')
})

test('binding sheet and source editor guard dirty drafts and lock while saving', async ({ page }) => {
  let providerSaveStatus = 503
  let releaseProviderSave: (() => void) | undefined
  const providerSaves: unknown[] = []
  await page.route('**/api/v1/web-search/config', (route) =>
    route.fulfill({
      json: {
        data: {
          revision: 1,
          enabled: false,
          backend: { kind: 'local', model_id: 'model-search' },
          max_turns: 6,
          total_time_seconds: 180,
          updated_at: '2026-09-01T00:00:00Z',
          limits: searchLimits,
        },
      },
    }),
  )
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({
      json: {
        data: [
          { id: 'model-search', model_id: 'search-model', display_name: 'Search model' },
          { id: 'model-second', model_id: 'second-model', display_name: 'Second model' },
        ],
      },
    }),
  )
  await page.route('**/api/v1/web-access/settings', (route) =>
    route.fulfill({ json: { data: { search_provider_ids: [], fetch_provider_ids: [] } } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [searchSource] } }))
  await page.route('**/api/v1/web-providers/source-exa', async (route) => {
    if (route.request().method() === 'PUT') {
      providerSaves.push(route.request().postDataJSON())
      await new Promise<void>((resolve) => {
        releaseProviderSave = resolve
      })
      if (providerSaveStatus !== 200) {
        await route.fulfill({ status: providerSaveStatus, json: { error: 'Fixture provider save unavailable' } })
        return
      }
    }
    await route.fulfill({ json: { data: searchSource } })
  })
  await page.goto('/web-search')
  const discard = page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })

  // 绑定 Sheet：未改动直接关闭；有草稿时询问，继续编辑保留，放弃后下次从确认值打开。
  const methodSheet = page.getByRole('dialog', { name: 'Edit search method' })
  const editMethod = page.getByRole('button', { name: 'Edit search method', exact: true })
  await editMethod.click()
  await methodSheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(methodSheet).toHaveCount(0)
  await editMethod.click()
  await methodSheet.locator('#search-local-model').click()
  await page.getByRole('option', { name: 'Second model' }).click()
  await page.keyboard.press('Escape')
  await expect(discard).toBeVisible()
  await discard.getByRole('button', { name: 'Keep editing' }).click()
  await expect(methodSheet.locator('#search-local-model')).toHaveText('Second model')
  await methodSheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await discard.getByRole('button', { name: 'Discard changes' }).click()
  await expect(methodSheet).toHaveCount(0)
  await editMethod.click()
  await expect(methodSheet.locator('#search-local-model')).toHaveText('Search model')
  await methodSheet.getByRole('button', { name: 'Cancel', exact: true }).click()

  // 来源编辑 Sheet：同样的离开保护。
  const sources = page.locator('#web-search-sources')
  const editor = page.getByRole('dialog', { name: 'Edit search service' })
  const name = editor.locator('#web-provider-name')
  const save = editor.getByRole('button', { name: 'Save service', exact: true })
  await sources.getByRole('button', { name: 'Edit', exact: true }).click()
  await editor.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(editor).toHaveCount(0)
  await sources.getByRole('button', { name: 'Edit', exact: true }).click()
  await name.fill('Renamed source')
  await editor.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(discard).toBeVisible()
  await discard.getByRole('button', { name: 'Keep editing' }).click()
  await expect(name).toHaveValue('Renamed source')
  await page.keyboard.press('Escape')
  await discard.getByRole('button', { name: 'Discard changes' }).click()
  await expect(editor).toHaveCount(0)
  await sources.getByRole('button', { name: 'Edit', exact: true }).click()
  await expect(name).toHaveValue('Search source')
  await name.fill('Renamed source')

  // 保存中锁定编辑与关闭；失败保留输入与错误，可重试。
  await save.click()
  await expect.poll(() => providerSaves.length).toBe(1)
  await expect(name).toBeDisabled()
  await expect(editor.getByRole('button', { name: 'Cancel', exact: true })).toBeDisabled()
  await page.keyboard.press('Escape')
  await expect(editor).toBeVisible()
  expect(providerSaves).toEqual([{ name: 'Renamed source', use_proxy: false }])
  releaseProviderSave?.()
  await expect(editor.getByRole('alert')).toContainText('Fixture provider save unavailable')
  await expect(name).toHaveValue('Renamed source')
  providerSaveStatus = 200
  await save.click()
  await expect.poll(() => providerSaves.length).toBe(2)
  releaseProviderSave?.()
  await expect(editor).toHaveCount(0)
})

test('search and page sources keep independent priorities with disabled boundary arrows', async ({ page }) => {
  const alpha: WebProvider = { ...searchSource, id: 'source-alpha', name: 'Alpha' }
  const beta: WebProvider = { ...searchSource, id: 'source-beta', name: 'Beta', kind: 'zhipu' }
  let rejectSave = true
  let saved: WebAccessSettings = { search_provider_ids: [alpha.id, beta.id], fetch_provider_ids: [beta.id, alpha.id] }
  const writes: WebAccessSettings[] = []
  await page.route('**/api/v1/web-search/config', (route) =>
    route.fulfill({
      json: {
        data: {
          revision: 1,
          enabled: false,
          backend: { kind: 'local', model_id: 'model-search' },
          max_turns: 6,
          total_time_seconds: 180,
          updated_at: '2026-09-01T00:00:00Z',
          limits: searchLimits,
        },
      },
    }),
  )
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({ json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [alpha, beta] } }))
  await page.route('**/api/v1/web-access/settings', async (route) => {
    if (route.request().method() === 'PUT') {
      writes.push(route.request().postDataJSON())
      if (rejectSave) {
        await route.fulfill({ status: 503, json: { error: 'Fixture ordering unavailable' } })
        return
      }
      saved = route.request().postDataJSON()
    }
    await route.fulfill({ json: { data: saved } })
  })
  await page.goto('/web-search')
  const sources = page.locator('#web-search-sources')
  const up = (name: string, capability: string) =>
    sources.getByRole('button', { name: `Move ${name} up in ${capability}`, exact: true })
  const down = (name: string, capability: string) =>
    sources.getByRole('button', { name: `Move ${name} down in ${capability}`, exact: true })

  await expect(up('Alpha', 'Search sources')).toBeDisabled()
  await expect(down('Beta', 'Search sources')).toBeDisabled()
  await expect(up('Beta', 'Page sources')).toBeDisabled()
  await expect(down('Alpha', 'Page sources')).toBeDisabled()

  // 失败：回到确认顺序，且不改变预算行草稿。
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  const turns = page.locator('#search-max-turns')
  await turns.fill('9')
  await down('Alpha', 'Search sources').click()
  await expect(sources.getByRole('alert')).toContainText('Fixture ordering unavailable')
  await expect(up('Alpha', 'Search sources')).toBeDisabled()
  await expect(turns).toHaveValue('9')

  rejectSave = false
  await down('Alpha', 'Search sources').click()
  await expect(up('Beta', 'Search sources')).toBeDisabled()
  await expect(down('Alpha', 'Search sources')).toBeDisabled()
  expect(saved).toEqual({ search_provider_ids: [beta.id, alpha.id], fetch_provider_ids: [beta.id, alpha.id] })
  // 网页读取顺序不受搜索顺序影响，反之亦然。
  await expect(up('Beta', 'Page sources')).toBeDisabled()
  await down('Beta', 'Page sources').click()
  await expect(up('Alpha', 'Page sources')).toBeDisabled()
  expect(saved).toEqual({ search_provider_ids: [beta.id, alpha.id], fetch_provider_ids: [alpha.id, beta.id] })
  expect(writes.at(-1)).toEqual(saved)
  await expect(turns).toHaveValue('9')
  await expect(page.getByRole('switch')).toHaveCount(1)
})

test('web search keeps one settings column with aligned actions and operable sources at every width', async ({
  page,
}) => {
  const alpha: WebProvider = { ...searchSource, id: 'source-alpha', name: 'Alpha', last_test_success: true }
  const beta: WebProvider = { ...searchSource, id: 'source-beta', name: 'Beta', kind: 'zhipu' }
  await page.route('**/api/v1/web-search/config', (route) =>
    route.fulfill({
      json: {
        data: {
          revision: 1,
          enabled: false,
          backend: { kind: 'local', model_id: 'model-search' },
          max_turns: 6,
          total_time_seconds: 180,
          updated_at: '2026-09-01T00:00:00Z',
          limits: searchLimits,
        },
      },
    }),
  )
  await page.route('**/api/v1/web-search/eligible-models', (route) =>
    route.fulfill({ json: { data: [{ id: 'model-search', model_id: 'search-model', display_name: 'Search model' }] } }),
  )
  await page.route('**/api/v1/web-providers', (route) => route.fulfill({ json: { data: [alpha, beta] } }))
  let savedSources: WebAccessSettings = { search_provider_ids: [alpha.id], fetch_provider_ids: [alpha.id] }
  await page.route('**/api/v1/web-access/settings', (route) => {
    if (route.request().method() === 'PUT') savedSources = route.request().postDataJSON()
    return route.fulfill({ json: { data: savedSources } })
  })
  await page.setViewportSize({ width: 2560, height: 1440 })
  await page.goto('/web-search')
  await page.getByRole('button', { name: 'Advanced', exact: true }).click()
  const remSize = await page.evaluate(() => parseFloat(getComputedStyle(document.documentElement).fontSize))
  // 设置行铺满设置列：列宽不超过 48rem，页头左缘与列对齐，设置列在内容容器中居中。
  const column = (await page.locator('[data-slot=setting-row]').first().boundingBox())!
  expect(column.width).toBeLessThanOrEqual(48 * remSize + 1)
  const header = (await page.getByRole('heading', { level: 1, name: 'Web search' }).boundingBox())!
  expect(Math.abs(column.x - header.x)).toBeLessThanOrEqual(1)
  const route = (await page.locator('.route-page').boundingBox())!
  expect(Math.abs(column.x - route.x - (route.x + route.width - column.x - column.width))).toBeLessThanOrEqual(1)
  const edit = (await page.getByRole('button', { name: 'Edit search method', exact: true }).boundingBox())!
  const steps = (await page.locator('#search-max-turns').boundingBox())!
  const suffix = (await page.locator('[data-slot=setting-row]').getByText('s', { exact: true }).boundingBox())!
  const editRight = edit.x + edit.width
  // 行内控件与区段动作共用设置列的右缘。
  expect(Math.abs(editRight - (column.x + column.width))).toBeLessThanOrEqual(1)
  const connect = (await page.getByRole('button', { name: 'Connect service', exact: true }).boundingBox())!
  expect(Math.abs(connect.x + connect.width - (column.x + column.width))).toBeLessThanOrEqual(1)
  expect(Math.abs(steps.x + steps.width - editRight)).toBeLessThanOrEqual(1)
  expect(Math.abs(suffix.x + suffix.width - editRight)).toBeLessThanOrEqual(1)
  expect(Math.abs(steps.width - 7 * remSize)).toBeLessThanOrEqual(1)
  const seconds = (await page.locator('#search-total-seconds').boundingBox())!
  expect(Math.abs(seconds.width - 7 * remSize)).toBeLessThanOrEqual(1)
  await expect(page.getByRole('switch')).toHaveCount(1)

  for (const width of [390, 320]) {
    await page.setViewportSize({ width, height: 844 })
    await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true)
    const sources = page.locator('#web-search-sources')
    for (const control of [
      sources.getByRole('button', { name: 'Test' }).first(),
      sources.getByRole('button', { name: 'Edit', exact: true }).first(),
      sources.getByRole('button', { name: 'Delete' }).first(),
      page.locator('#web-access-search-source-beta'),
      page.locator('#web-access-fetch-source-beta'),
    ]) {
      await control.scrollIntoViewIfNeeded()
      const box = (await control.boundingBox())!
      expect(box.x).toBeGreaterThanOrEqual(0)
      expect(box.x + box.width).toBeLessThanOrEqual(width)
    }
    await expect(sources.getByRole('button', { name: 'Move Alpha down in Search sources', exact: true })).toBeDisabled()
    // 说明在任何宽度都只在问号里，不常驻成小字。
    await page.mouse.move(0, 0)
    await expect(page.getByText('Allowed range', { exact: false })).toHaveCount(0)
    const hint = page.locator('[data-slot=field-hint]').first()
    await hint.scrollIntoViewIfNeeded()
    await hint.hover()
    await expect(page.locator('[data-slot=tooltip-content]').first()).toContainText('Allowed range: 1–20.')
  }
  await page.locator('#web-access-search-source-beta').click()
  await expect(page.locator('#web-access-search-source-beta')).toBeChecked()
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
  test(`disabled update checks show one status and no controls to retry (${locale})`, async ({ page }) => {
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
    // 开发版只说明一次“不支持在线检查更新”：没有失效的检查按钮、失败记录或代理开关。
    await expect(updates.getByRole('status')).toHaveCount(1)
    await expect(updates.getByRole('status')).toBeVisible()
    await expect(updates.getByRole('button')).toHaveCount(0)
    await expect(updates.getByRole('switch')).toHaveCount(0)
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
  await page.route('**/api/v1/reversible-redaction/discoveries?**', (route) =>
    route.fulfill({ json: { data: { items: [], next_cursor: null } satisfies CredentialDiscoveryPage } }),
  )
  await page.goto('/reversible-redaction')
  await page.getByRole('tab', { name: /Current rules/ }).click()
  const table = page.getByRole('table')
  await expect(table.getByRole('button', { name: /Fixture credential 0/ })).toBeVisible()
  const header = table.locator('thead')
  const scrollArea = page.locator('[data-slot="data-table-viewport"]')
  const viewport = scrollArea.locator('[data-scroll-area-viewport]')
  const main = page.getByRole('main')
  const paginator = page.locator('[data-slot="data-table-paginator"]')
  // 规则区占满窗口剩余高度：任何窗口高度下分页器都贴在底部，表体内部滚动。
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
  await expect
    .poll(async () => {
      const viewportBox = (await viewport.boundingBox())!
      const barBox = (await bar.boundingBox())!
      return barBox.x - viewportBox.x - viewportBox.width
    })
    .toBeGreaterThanOrEqual(0)
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
                  created: [
                    { rule_id: 'fixture-rule-0', source_type: 'user_message', latest_input: true },
                    { rule_id: 'fixture-rule-0', source_type: 'tool_arguments' },
                  ],
                  reused: [{ rule_id: 'fixture-rule-0', source_type: 'user_message', latest_input: false }],
                  status: 'failed',
                  observation_gap: false,
                },
              ],
              next_cursor: cursor ? null : 'next-fixture',
            },
          },
        })
      } else if (url.pathname.endsWith('/custom-rules')) {
        await route.fulfill({ json: { data: [] } })
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
    // 命中记录是第一个也是默认的页签：每条命中直接显示规则、位置与请求状态。
    await expect(page.getByRole('tab').first()).toHaveText(zh ? /命中记录/ : /Hit records/)
    const record = page.getByRole('row').filter({ has: page.getByRole('cell', { name: 'Fixture key', exact: true }) })
    const chipRule = zh ? 'Fixture credential 0 出现在' : 'Fixture credential 0 found in'
    await expect(record.getByRole('listitem')).toHaveCount(3)
    await expect(record.getByRole('button', { name: `${chipRule} ${zh ? '用户消息' : 'User message'}` })).toBeVisible()
    await expect(
      record.getByRole('button', { name: `${chipRule} ${zh ? '工具参数' : 'Tool arguments'}` }),
    ).toBeVisible()
    await expect(
      record.getByRole('button', { name: `${chipRule} ${zh ? '历史用户消息' : 'Earlier user message'}` }),
    ).toBeVisible()
    await expect(record.getByRole('status')).toHaveText(zh ? '失败' : 'Failed')
    await expect(
      record.getByRole('link', { name: zh ? '查看关联请求记录' : 'Open request observation' }),
    ).toHaveAttribute('href', '/logs?interaction=interaction-atlas&highlight=credential-hits')
    // 第二页随滚动自动加载，不再有「加载更多」按钮。
    await expect(page.getByRole('row').filter({ hasText: 'Second fixture key' })).toBeVisible()
    expect(cursors).toEqual([null, 'next-fixture'])
    await page.getByRole('tab', { name: new RegExp(zh ? '现有规则' : 'Current rules') }).click()
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
    } else if (path.endsWith('/custom-rules')) {
      await route.fulfill({ json: { data: [] } })
    } else {
      await route.fulfill({ json: { data: { items: [], next_cursor: null } } })
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
  const timeline = page.getByRole('region', { name: 'Hit records' })
  await expect(catalog.getByRole('alert')).toContainText('Rules could not be loaded.', { timeout: 15_000 })
  unavailable = false
  await catalog.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(catalog.locator('#credential-rule-search')).toBeVisible()
  await page.getByRole('tab', { name: 'Hit records', exact: true }).click()
  await expect(timeline.getByRole('alert')).toContainText('Hit records are unavailable.')
  await expect(timeline.getByText('No credential hits in the available observations.')).toHaveCount(0)
  await timeline.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(timeline.getByText('No credential hits in the available observations.')).toBeVisible()
  await expect(timeline.getByRole('alert')).toHaveCount(0)
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
    const pathname = new URL(route.request().url()).pathname
    const rules = pathname.endsWith('/rules') && !pathname.endsWith('/custom-rules')
    await route.fulfill({
      json: {
        data: rules
          ? { rules: [], prefilter: '', filter: '' }
          : pathname.endsWith('/custom-rules')
            ? []
            : { items: [], next_cursor: null },
      },
    })
  })
  await page.goto('/reversible-redaction')
  await expect(
    page.getByRole('alert').filter({ hasText: 'Credential protection settings could not be loaded.' }),
  ).toBeVisible({ timeout: 15_000 })
  await expect(page.getByRole('switch')).toBeDisabled()
  await expect(page.getByText('No credential hits in the available observations.', { exact: true })).toBeVisible()
  await page.getByRole('tab', { name: /Current rules/ }).click()
  await expect(page.getByText('No rules are bundled with this instance.', { exact: true })).toBeVisible()
  settingsUnavailable = false
  await page.getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(page.getByRole('switch')).toBeEnabled()
  await expect(page.getByRole('switch')).not.toBeChecked()
})

const discardDialog = (page: Page) => page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })

test('a settings Sheet closes freely when clean and confirms before discarding a draft', async ({ page }) => {
  let saved = { url: 'http://127.0.0.1:7890', bypass: '', force_http1: true }
  const writes: unknown[] = []
  await page.route('**/api/v1/settings/outbound_proxy', async (route) => {
    if (route.request().method() === 'PUT') {
      writes.push(route.request().postDataJSON())
      saved = JSON.parse(route.request().postDataJSON().value)
    }
    await route.fulfill({ json: { data: JSON.stringify(saved) } })
  })
  await page.goto('/settings?view=network')
  const sheet = page.getByRole('dialog', { name: 'Edit proxy' })
  const url = page.locator('#proxy-url')

  // No draft: Escape closes without asking.
  await page.getByRole('button', { name: /^Edit/ }).click()
  await expect(sheet).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(sheet).toHaveCount(0)
  await expect(discardDialog(page)).toHaveCount(0)

  // Draft: Escape asks; keeping the draft leaves the Sheet and the text untouched.
  await page.getByRole('button', { name: /^Edit/ }).click()
  await url.fill('socks5h://127.0.0.1:1080')
  await page.keyboard.press('Escape')
  await expect(discardDialog(page)).toBeVisible()
  await discardDialog(page).getByRole('button', { name: 'Keep editing' }).click()
  await expect(discardDialog(page)).toHaveCount(0)
  await expect(sheet).toBeVisible()
  await expect(url).toHaveValue('socks5h://127.0.0.1:1080')

  // Cancel goes through the same confirmation; discarding closes and reopening starts from the saved value.
  await sheet.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(discardDialog(page)).toBeVisible()
  await discardDialog(page).getByRole('button', { name: 'Discard changes' }).click()
  await expect(sheet).toHaveCount(0)
  expect(writes).toHaveLength(0)
  await page.getByRole('button', { name: /^Edit/ }).click()
  await expect(url).toHaveValue('http://127.0.0.1:7890')

  // Reverting the draft by hand makes it clean again.
  await url.fill('http://127.0.0.1:1')
  await url.fill('http://127.0.0.1:7890')
  await page.keyboard.press('Escape')
  await expect(sheet).toHaveCount(0)
  await expect(discardDialog(page)).toHaveCount(0)
})

test('a settings Sheet is locked while saving and closes without asking after a successful save', async ({ page }) => {
  let saved = { url: 'http://127.0.0.1:7890', bypass: '', force_http1: true }
  let release: (() => void) | undefined
  await page.route('**/api/v1/settings/outbound_proxy', async (route) => {
    if (route.request().method() === 'PUT') {
      await new Promise<void>((resolve) => (release = resolve))
      saved = JSON.parse(route.request().postDataJSON().value)
    }
    await route.fulfill({ json: { data: JSON.stringify(saved) } })
  })
  await page.goto('/settings?view=network')
  const sheet = page.getByRole('dialog', { name: 'Edit proxy' })
  await page.getByRole('button', { name: /^Edit/ }).click()
  await page.locator('#proxy-url').fill('http://127.0.0.1:9999')
  await sheet.getByRole('button', { name: 'Save proxy', exact: true }).click()
  await expect.poll(() => release !== undefined).toBe(true)
  await page.keyboard.press('Escape')
  await expect(sheet).toBeVisible()
  await expect(discardDialog(page)).toHaveCount(0)
  release?.()
  await expect(sheet).toHaveCount(0)
  await expect(discardDialog(page)).toHaveCount(0)
  await expect(page.locator('#proxy')).toContainText('http://127.0.0.1:9999')
})

test('switching settings groups with a dirty inline input asks first and stays quiet when clean', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 })
  await page.route('**/api/v1/settings/outbound_proxy', (route) =>
    route.fulfill({ json: { data: JSON.stringify({ url: '', bypass: 'localhost', force_http1: true }) } }),
  )
  await page.goto('/settings?view=network')
  const nav = page.getByRole('navigation', { name: 'Settings sections' })
  const bypass = page.locator('#proxy-bypass')

  // Clean: no prompt.
  await nav.getByRole('link', { name: 'Data' }).click()
  await expect(page).toHaveURL(/view=data$/)
  await expect(discardDialog(page)).toHaveCount(0)
  await nav.getByRole('link', { name: 'Network' }).click()
  await expect(page).toHaveURL(/view=network$/)

  // Dirty: the switch is held until the draft is discarded; keeping it leaves group and text unchanged.
  await bypass.fill('localhost,.internal')
  await nav.getByRole('link', { name: 'Data' }).click()
  await expect(discardDialog(page)).toBeVisible()
  await expect(page).toHaveURL(/view=network$/)
  await discardDialog(page).getByRole('button', { name: 'Keep editing' }).click()
  await expect(page).toHaveURL(/view=network$/)
  await expect(bypass).toHaveValue('localhost,.internal')

  await nav.getByRole('link', { name: 'Data' }).click()
  await discardDialog(page).getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/view=data$/)
  await expect(page.getByRole('heading', { level: 2, name: 'Request history' })).toBeVisible()

  // The discarded draft is gone, and leaving the page is guarded the same way once dirty again.
  await nav.getByRole('link', { name: 'Network' }).click()
  await expect(bypass).toHaveValue('localhost')
  await bypass.fill('changed')
  await page.getByRole('navigation', { name: 'Primary navigation' }).getByRole('link', { name: 'Models' }).click()
  await expect(discardDialog(page)).toBeVisible()
  await expect(page).toHaveURL(/\/settings\?view=network$/)
  await discardDialog(page).getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/\/models$/)

  // Saving clears the draft: nothing to confirm afterwards.
  await page.goto('/settings?view=network')
  await bypass.fill('saved-host')
  await bypass.press('Enter')
  await expect(page.locator('#proxy').getByRole('status')).toHaveText('Saved')
  await nav.getByRole('link', { name: 'Data' }).click()
  await expect(page).toHaveURL(/view=data$/)
  await expect(discardDialog(page)).toHaveCount(0)
})
