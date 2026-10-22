import { expect, test, type Page, type Route } from '@playwright/test'

import type { ProviderModelDetail, ProviderModelSummary } from '../src/lib/types'
import { prepareApp } from './prepare-app'

const providerId = 'editing-provider'
const basePath = `/providers/${providerId}`
const timestamp = '2026-09-01T00:00:00Z'

function deferred() {
  let resolve!: () => void
  const promise = new Promise<void>((done) => {
    resolve = done
  })
  return { promise, resolve }
}

function model(id: string, name: string): ProviderModelDetail & ProviderModelSummary {
  return {
    id,
    name,
    available: true,
    source_kind: 'manual',
    snapshot_state: { type: 'edited', source: null },
    selection_policy: 'auto',
    specification: { limit: { context: 128000 }, modalities: null },
    revision: 1,
    can_reimport: false,
    metadata: { id, name, description: `${name} specification`, limit: { context: 128000 } },
    extensions: {},
    created_at: timestamp,
    updated_at: timestamp,
  }
}

type Detail = ProviderModelDetail
type Fixture = {
  details: Record<string, Detail>
  reads: string[]
  writes: Record<string, unknown>[]
  selections: Record<string, unknown>[]
  read?: (route: Route, detail: Detail) => Promise<void>
  save?: (route: Route, detail: Detail, body: Record<string, unknown>) => Promise<void>
  selection?: (route: Route, detail: Detail, body: Record<string, unknown>) => Promise<void>
  failInventory: boolean
  inventoryReads: number
}

async function setup(page: Page): Promise<Fixture> {
  await prepareApp(page)
  await page.setViewportSize({ width: 1920, height: 1080 })
  const provider = {
    id: providerId,
    name: 'Editing Provider',
    vendor: 'openai',
    protocol: 'openai-compatible',
    base_url: 'https://fixture.invalid/v1',
    use_proxy: false,
    channel: 'default',
    vendor_options: {},
    configured_credential_fields: ['api_key'],
    is_enabled: true,
    created_at: timestamp,
    updated_at: timestamp,
  }
  const fixture: Fixture = {
    details: { 'model-a': model('model-a', 'Alpha Model'), 'model-b': model('model-b', 'Beta Model') },
    reads: [],
    writes: [],
    selections: [],
    failInventory: false,
    inventoryReads: 0,
  }
  await page.route('**/api/v1/models', (route) => route.fulfill({ json: { data: [] } }))
  await page.route('**/api/v1/providers**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname.replace('/api/v1', '')
    if (request.method() === 'GET' && path === '/providers') {
      await route.fulfill({ json: { data: [provider] } })
    } else if (request.method() === 'GET' && path === basePath) {
      await route.fulfill({ json: { data: provider } })
    } else if (request.method() === 'GET' && path === `${basePath}/models`) {
      fixture.inventoryReads += 1
      await route.fulfill(
        fixture.failInventory
          ? { status: 503, json: { error: 'Fixture inventory refresh unavailable' } }
          : {
              json: {
                data: {
                  models: Object.values(fixture.details).map((detail) => ({
                    ...detail,
                    name: detail.metadata.name,
                    specification: { limit: detail.metadata.limit, modalities: null },
                  })),
                },
              },
            },
      )
    } else if (request.method() === 'GET' && path === `${basePath}/model`) {
      const id = url.searchParams.get('model')!
      fixture.reads.push(id)
      const detail = fixture.details[id]
      if (fixture.read) await fixture.read(route, detail)
      else await route.fulfill({ json: { data: detail } })
    } else if (request.method() === 'PUT' && path === `${basePath}/model`) {
      const body = request.postDataJSON() as Record<string, unknown>
      fixture.writes.push(body)
      const detail =
        fixture.details[url.searchParams.get('model') ?? String(body.model_id)] ?? fixture.details['model-a']
      if (fixture.save) await fixture.save(route, detail, body)
      else await route.fulfill({ json: { data: detail } })
    } else if (request.method() === 'PUT' && path === `${basePath}/model/selection`) {
      const body = request.postDataJSON() as Record<string, unknown>
      fixture.selections.push(body)
      const detail = fixture.details['model-a']
      if (fixture.selection) await fixture.selection(route, detail, body)
      else {
        detail.selection_policy = body.policy as Detail['selection_policy']
        detail.revision += 1
        await route.fulfill({ json: { data: detail } })
      }
    } else {
      await route.fallback()
    }
  })
  await page.goto(`${basePath}?view=models`)
  await expect(page.getByRole('heading', { name: provider.name })).toBeVisible()
  return fixture
}

async function openModel(page: Page, name: string) {
  await page.getByRole('row').filter({ hasText: name }).getByRole('cell').filter({ hasText: name }).click()
}

for (const surface of ['detail', 'cell'] as const) {
  test(`${surface} modality editing keeps custom choices available and saves independent directions`, async ({
    page,
  }) => {
    const fixture = await setup(page)
    fixture.details['model-a'].metadata.modalities = { input: ['text', 'binary'], output: ['text'] }
    await page.route('**/api/v1/settings/rpm_admission', (route) => route.fulfill({ json: { data: null } }))
    if (surface === 'detail') await openModel(page, 'Alpha Model')
    else await page.getByRole('button', { name: 'Modalities model-a', exact: true }).click()

    const input = page.locator('[data-modality-select="input"]')
    await input.click()
    const custom = page.getByRole('option', { name: 'binary', exact: true })
    await custom.click()
    await expect(custom).toBeVisible()
    await expect(custom).not.toHaveAttribute('aria-selected', 'true')
    await custom.click()
    await expect(custom).toHaveAttribute('aria-selected', 'true')
    await page.getByRole('option', { name: 'Audio', exact: true }).click()
    await page.keyboard.press('Escape')
    await page.locator('[data-modality-select="output"]').click()
    await page.getByRole('option', { name: 'Image', exact: true }).click()
    await page.keyboard.press('Escape')
    await page.getByRole('button', { name: surface === 'detail' ? 'Save model' : 'Save', exact: true }).click()
    await expect
      .poll(() => fixture.writes.map((write) => (write.metadata as Detail['metadata']).modalities))
      .toEqual([{ input: ['text', 'audio', 'binary'], output: ['text', 'image'] }])
  })
}

test('cell editors keep optional explanations behind help hints', async ({ page }) => {
  await setup(page)
  await page.getByRole('button', { name: 'Reasoning effort model-a', exact: true }).click()
  const popover = page.getByRole('dialog')
  await expect(popover.getByLabel('Reasoning effort', { exact: true })).toBeVisible()
  await expect(popover.getByText('Record the reasoning efforts this model accepts')).toBeHidden()
  await popover.locator('[data-slot="field-hint"]').hover()
  await expect(page.locator('[data-slot="tooltip-content"]').first()).toContainText(
    'Record the reasoning efforts this model accepts',
  )
  await page.keyboard.press('Escape')
  await page.keyboard.press('Escape')

  await page.getByRole('button', { name: 'Availability when adding models model-a', exact: true }).click()
  await expect(page.getByRole('dialog').getByText('Controls whether this upstream model can be chosen')).toBeHidden()
  await page.getByRole('dialog').locator('[data-slot="field-hint"]').hover()
  await expect(page.locator('[data-slot="tooltip-content"]').first()).toContainText(
    'Controls whether this upstream model can be chosen',
  )
  await expect(page.locator('[data-slot="tooltip-content"]').first()).not.toContainText('immediately')
})

const discardDialog = (page: Page) => page.getByRole('alertdialog', { name: 'Discard unsaved model changes?' })
const nameField = (page: Page) => page.locator('#provider-model-name')

test('detail footer saves an RPM-only change once without rewriting the model specification', async ({ page }) => {
  const fixture = await setup(page)
  const rpmWrites: unknown[] = []
  await page.route('**/api/v1/settings/rpm_admission', async (route) => {
    if (route.request().method() === 'PUT') {
      rpmWrites.push(JSON.parse(route.request().postDataJSON().value))
      await route.fulfill({ json: { data: null } })
    } else await route.fulfill({ json: { data: null } })
  })
  await openModel(page, 'Alpha Model')
  const rpm = page.getByRole('region', { name: 'RPM limit', exact: true })
  await expect(rpm.getByRole('button')).toHaveCount(0)
  await rpm.getByRole('spinbutton', { name: 'Requests per minute', exact: true }).fill('12')
  await page.getByRole('button', { name: 'Save model', exact: true }).click()
  await expect.poll(() => rpmWrites.length).toBe(1)
  expect(rpmWrites[0]).toMatchObject({ destinations: [{ provider_id: providerId, model: 'model-a', rpm_limit: 12 }] })
  expect(fixture.writes).toEqual([])
})

test('late Alpha detail cannot replace Beta after an in-app model switch', async ({ page }) => {
  const fixture = await setup(page)
  const gate = deferred()
  fixture.read = async (route, detail) => {
    if (detail.id === 'model-a') await gate.promise
    await route.fulfill({ json: { data: detail } })
  }
  await openModel(page, 'Alpha Model')
  await expect.poll(() => fixture.reads).toEqual(['model-a'])
  await page.evaluate(() => history.back())
  await expect(page).toHaveURL(`${basePath}?view=models`)
  await openModel(page, 'Beta Model')
  await expect(nameField(page)).toHaveValue('Beta Model')
  const lateResponse = page.waitForResponse(
    (response) => new URL(response.url()).searchParams.get('model') === 'model-a',
  )
  gate.resolve()
  await (await lateResponse).finished()
  await expect(nameField(page)).toHaveValue('Beta Model')
  await expect(page).toHaveURL(/model=model-b/)
  await expect(nameField(page)).toBeEnabled()
})

test('dirty Back, Forward and internal navigation preserve or discard the draft as confirmed', async ({ page }) => {
  await setup(page)
  await openModel(page, 'Alpha Model')
  await nameField(page).fill('Unsaved Alpha')
  await page.evaluate(() => history.back())
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Keep editing' }).click()
  await expect(nameField(page)).toHaveValue('Unsaved Alpha')
  await expect(page).toHaveURL(/model=model-a/)
  await page.evaluate(() => history.back())
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(`${basePath}?view=models`)
  await page.evaluate(() => history.forward())
  await expect(nameField(page)).toHaveValue('Alpha Model')
  await nameField(page).fill('Unsaved Alpha')
  await page
    .getByRole('navigation', { name: 'Provider details' })
    .getByRole('link', { name: 'Connection settings' })
    .click()
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Keep editing' }).click()
  await expect(nameField(page)).toHaveValue('Unsaved Alpha')
  await page
    .getByRole('navigation', { name: 'Provider details' })
    .getByRole('link', { name: 'Connection settings' })
    .click()
  await page.getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/view=connection/)
  await expect(nameField(page)).toHaveCount(0)
  await page.evaluate(() => history.back())
  await expect(nameField(page)).toHaveValue('Alpha Model')
  await nameField(page).fill('Unsaved forward draft')
  await page.evaluate(() => history.forward())
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Keep editing' }).click()
  await expect(nameField(page)).toHaveValue('Unsaved forward draft')
  await expect(page).toHaveURL(/model=model-a/)
  await page.evaluate(() => history.forward())
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Discard changes' }).click()
  await expect(page).toHaveURL(/view=connection/)
})

async function chooseManualTemplate(page: Page) {
  await page.getByRole('button', { name: 'Add manually', exact: true }).click()
  await page.locator('#manual-provider-model-search').click()
  await page.getByPlaceholder('Search model template').fill('openai/gpt-5.4')
  await page.getByRole('option', { name: /GPT-5\.4.*openai\/gpt-5\.4/ }).click()
  await page.getByRole('button', { name: 'Continue' }).click()
}

test('mobile manual drawer retains dirty fields on Keep and closes only after confirmed discard', async ({ page }) => {
  await setup(page)
  await page.route(`**/api/v1/providers/${providerId}/model/prepare`, (route) =>
    route.fulfill({ json: { data: model('gpt-5.4', 'Manual Model') } }),
  )
  await chooseManualTemplate(page)
  await expect(nameField(page)).toHaveValue('Manual Model')
  await page.setViewportSize({ width: 390, height: 844 })
  await nameField(page).fill('Unsaved manual draft')
  await page.getByRole('button', { name: 'Close model editor' }).click()
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Keep editing' }).click()
  await expect(nameField(page)).toHaveValue('Unsaved manual draft')
  await page.keyboard.press('Escape')
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Discard changes' }).click()
  await expect(nameField(page)).toHaveCount(0)
  await expect(page).not.toHaveURL(/model=/)
})

test('cancelling manual preparation prevents late results from reopening the editor', async ({ page }) => {
  await setup(page)
  const gate = deferred()
  let preparing = false
  await page.route(`**/api/v1/providers/${providerId}/model/prepare`, async (route) => {
    preparing = true
    await gate.promise
    await route.fulfill({ json: { data: model('gpt-5.4', 'Manual Model') } })
  })
  await chooseManualTemplate(page)
  await expect.poll(() => preparing).toBe(true)
  await page.getByRole('dialog', { name: 'Add a model manually' }).getByRole('button', { name: 'Cancel' }).click()
  const lateResponse = page.waitForResponse((response) => response.url().endsWith('/model/prepare'))
  gate.resolve()
  await (await lateResponse).finished()
  await expect(nameField(page)).toHaveCount(0)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  await expect(page).not.toHaveURL(/model=/)
})

test('pending save locks fields, availability, Cancel, repeated save, and browser navigation', async ({ page }) => {
  const fixture = await setup(page)
  const gate = deferred()
  fixture.save = async (route, detail, body) => {
    await gate.promise
    await route.fulfill({ json: { data: { ...detail, metadata: body.metadata, revision: 2 } } })
  }
  await openModel(page, 'Alpha Model')
  await nameField(page).fill('Saved Alpha')
  await page.getByRole('button', { name: 'Save model' }).click()
  await expect.poll(() => fixture.writes.length).toBe(1)
  await expect(nameField(page)).toBeDisabled()
  await expect(page.getByLabel('Availability when adding models')).toBeDisabled()
  await expect(page.getByRole('button', { name: 'Cancel', exact: true })).toBeDisabled()
  await expect(page.getByRole('button', { name: 'Save model' })).toBeDisabled()
  await page.evaluate(() => history.back())
  await expect(page).toHaveURL(/model=model-a/)
  await expect(discardDialog(page)).toHaveCount(0)
  gate.resolve()
  await expect(nameField(page)).toBeEnabled()
  await expect(nameField(page)).toHaveValue('Saved Alpha')
  expect(fixture.writes).toHaveLength(1)
})

test('availability writes preserve unsaved specifications and advance the next save revision', async ({ page }) => {
  const fixture = await setup(page)
  await openModel(page, 'Alpha Model')
  await nameField(page).fill('Unsaved specification')
  await page.getByLabel('Availability when adding models').click()
  await page.getByRole('option', { name: "Don't allow" }).click()
  await expect.poll(() => fixture.selections.length).toBe(1)
  await expect(page.getByLabel('Availability when adding models')).toBeEnabled()
  await expect(nameField(page)).toHaveValue('Unsaved specification')
  await page.getByRole('button', { name: 'Cancel', exact: true }).click()
  await expect(discardDialog(page)).toBeVisible()
  await page.getByRole('button', { name: 'Keep editing' }).click()
  fixture.save = async (route, detail, body) => {
    await route.fulfill({ json: { data: { ...detail, metadata: body.metadata, revision: 3 } } })
  }
  await page.getByRole('button', { name: 'Save model' }).click()
  await expect.poll(() => fixture.writes.length).toBe(1)
  expect(fixture.writes[0]).toMatchObject({ revision: 2, metadata: { name: 'Unsaved specification' } })
  await expect(nameField(page)).toBeEnabled()
})

test('saved revision survives refresh failure and Retry refreshes without repeating the write', async ({ page }) => {
  const fixture = await setup(page)
  fixture.save = async (route, detail, body) => {
    const saved = { ...detail, metadata: body.metadata as Detail['metadata'], revision: 7 }
    fixture.details[detail.id] = saved
    fixture.failInventory = true
    await route.fulfill({ json: { data: saved } })
  }
  await openModel(page, 'Alpha Model')
  await nameField(page).fill('Authoritative saved name')
  await page.getByRole('button', { name: 'Save model' }).click()
  const warning = page.getByText('Model changes were saved, but related data could not be refreshed.')
  await expect(warning).toBeVisible()
  await expect(nameField(page)).toHaveValue('Authoritative saved name')
  const readsBeforeRetry = fixture.inventoryReads
  fixture.failInventory = false
  await page.getByRole('alert').filter({ has: warning }).getByRole('button', { name: 'Retry', exact: true }).click()
  await expect(warning).toHaveCount(0)
  await expect.poll(() => fixture.inventoryReads).toBeGreaterThan(readsBeforeRetry)
  expect(fixture.writes).toHaveLength(1)
  await nameField(page).fill('Next revision')
  fixture.save = async (route, detail, body) => {
    await route.fulfill({ json: { data: { ...detail, metadata: body.metadata, revision: 8 } } })
  }
  await page.getByRole('button', { name: 'Save model' }).click()
  await expect.poll(() => fixture.writes.length).toBe(2)
  expect(fixture.writes[1]).toMatchObject({ revision: 7, metadata: { name: 'Next revision' } })
  await expect(nameField(page)).toBeEnabled()
})
