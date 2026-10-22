import { expect, test } from '@playwright/test'
import type { ModelChangeRecord, ProviderModelSyncSummary } from '../src/lib/types'
import { prepareApp } from './prepare-app'
const checked = '2026-10-12T10:00:00.000Z'
const provider = {
  id: 'history-provider',
  name: 'History Service',
  vendor: 'openai',
  channel: 'default',
  protocol: 'openai-compatible',
  base_url: 'https://fixture.invalid',
  use_proxy: false,
  is_enabled: true,
  created_at: checked,
  updated_at: checked,
}
const summary: ProviderModelSyncSummary = {
  checked_at: checked,
  source: 'manual_sync',
  record_id: 'exact-result',
  added: 0,
  missing: 0,
  restored: 0,
  deprecated: 0,
  deprecation_cleared: 1,
  specification_updated: 1,
  outcome: 'success',
  error: null,
}
const exact: ModelChangeRecord = {
  id: 'exact-result',
  provider_id: provider.id,
  provider_name: provider.name,
  provider_deleted: false,
  source: 'manual_sync',
  checked_at: checked,
  outcome: 'success',
  initial_sync: false,
  error: null,
  changes: [
    {
      model_id: 'exact-model',
      name: 'Exact Model',
      types: ['deprecation_cleared', 'specification_updated'],
      specification: [
        { field: 'context', before: null, after: 0 },
        { field: 'input_modalities', before: ['text'], after: [] },
      ],
      before_presence: 'present',
      after_presence: 'present',
      before_deprecated: true,
      after_deprecated: false,
      before_policy: 'auto',
      after_policy: 'auto',
    },
  ],
}
test('late automatic preference response cannot change another service', async ({ page }) => {
  await prepareApp(page)
  const other = { ...provider, id: 'other-provider', name: 'Other Service' }
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider, other] } }))
  await page.route('**/api/v1/providers/*/models', (route) => route.fulfill({ json: { data: { models: [] } } }))
  let release!: () => void
  const gate = new Promise<void>((done) => (release = done))
  let entered = false
  await page.route('**/api/v1/providers/*/models/sync-state', async (route) => {
    const saving = route.request().method() === 'PUT'
    if (saving) {
      entered = true
      await gate
    }
    await route.fulfill({
      json: {
        data: {
          automatic_sync: !saving,
          in_progress: false,
          pause_reason: null,
          last_check: null,
          has_successful_sync: true,
        },
      },
    })
  })
  await page.goto(`/providers/${provider.id}?view=models`)
  await page.getByRole('switch', { name: 'Automatic sync' }).click()
  await expect.poll(() => entered).toBe(true)
  // 使用同一路由的客户端导航，保留组件实例而不通过文档重载取消旧请求。
  await page.evaluate((id) => {
    const link = document.createElement('a')
    link.href = `/providers/${id}?view=models`
    link.textContent = 'Open Other Service'
    document.body.append(link)
  }, other.id)
  await page.getByRole('link', { name: 'Open Other Service' }).click()
  await expect(page.getByRole('heading', { name: other.name })).toBeVisible()
  const toggle = page.getByRole('switch', { name: 'Automatic sync' })
  await expect(toggle).toBeChecked()
  const settled = page.waitForResponse(
    (response) => response.request().method() === 'PUT' && response.url().includes(provider.id),
  )
  release()
  await (await settled).finished()
  await expect(toggle).toBeEnabled()
  await page.evaluate(() => new Promise<void>((done) => requestAnimationFrame(() => done())))
  expect(await toggle.isChecked()).toBe(true)
})
for (const outcome of ['failure', 'partial_success'] as const) {
  test(`sync retry remains bound to its original service after navigation (${outcome})`, async ({ page }) => {
    await prepareApp(page)
    const other = { ...provider, id: 'other-provider', name: 'Other Service' }
    await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider, other] } }))
    await page.route('**/api/v1/providers/*/models', (route) => route.fulfill({ json: { data: { models: [] } } }))
    const attempted: string[] = []
    await page.route('**/api/v1/providers/*/models/sync', (route) => {
      attempted.push(new URL(route.request().url()).pathname.split('/')[4])
      const result = { ...summary, outcome, error: 'Synthetic discovery failure' }
      return route.fulfill(
        outcome === 'failure'
          ? { status: 502, json: { error: result.error, details: { sync_summary: result } } }
          : { json: { data: result } },
      )
    })
    await page.goto(`/providers/${provider.id}?view=models`)
    await page.getByRole('button', { name: 'Sync models', exact: true }).first().click()
    const notification = page
      .locator('[data-sonner-toast]')
      .filter({ hasText: outcome === 'failure' ? 'Model sync failed' : 'Model changes saved, but a later step failed' })
    await expect(notification).toBeVisible()
    await page.evaluate((id) => {
      const link = document.createElement('a')
      link.href = `/providers/${id}?view=models`
      link.textContent = 'Open Other Service'
      document.body.append(link)
    }, other.id)
    await page.getByRole('link', { name: 'Open Other Service' }).click()
    await expect(page.getByRole('heading', { name: other.name })).toBeVisible()
    await notification.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect.poll(() => attempted).toEqual([provider.id, provider.id])
    await expect(page).toHaveURL(new RegExp(`/providers/${other.id}\\?view=models$`))
    await expect(page.getByRole('button', { name: 'Try again', exact: true })).toHaveCount(0)
  })
}
for (const destination of ['connection', 'providers', 'narrow-settings']) {
  test(`toast history remains reachable after navigation during sync (${destination})`, async ({ page }) => {
    await prepareApp(page)
    await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
    await page.route('**/api/v1/providers/history-provider/models', (route) =>
      route.fulfill({ json: { data: { models: [] } } }),
    )
    let release!: () => void
    const gate = new Promise<void>((done) => (release = done))
    let entered = false
    await page.route('**/api/v1/providers/history-provider/models/sync', async (route) => {
      entered = true
      await gate
      await route.fulfill({ json: { data: summary } })
    })
    await page.route('**/api/v1/model-changes?*', (route) =>
      route.fulfill({ json: { data: { records: [], next_cursor: null } } }),
    )
    await page.route('**/api/v1/model-changes/exact-result', (route) => route.fulfill({ json: { data: exact } }))
    await page.goto(`/providers/${provider.id}?view=models`)
    await page.getByRole('button', { name: 'Sync models', exact: true }).first().click()
    await expect.poll(() => entered).toBe(true)
    if (destination === 'connection') {
      await page.getByRole('link', { name: 'Connection settings', exact: true }).click()
      await expect(page).toHaveURL(/view=connection/)
    } else if (destination === 'providers') {
      await page.getByRole('link', { name: 'Providers', exact: true }).first().click()
      await expect(page).toHaveURL(/\/providers$/)
    } else {
      await page.getByRole('link', { name: 'Settings', exact: true }).click()
      await expect(page).toHaveURL(/\/settings$/)
      await page.setViewportSize({ width: 320, height: 740 })
    }
    release()
    const toast = page.locator('[data-sonner-toast]').filter({ hasText: 'Model changes saved' })
    await toast.getByRole('button', { name: 'View details' }).focus()
    await page.keyboard.press('Enter')
    const panel = page.getByRole('dialog', { name: 'Change history' })
    await expect(panel).toBeVisible()
    const selected = panel.getByLabel('View details', { exact: true })
    await expect(selected).toContainText('exact-model')
    await expect(selected.getByRole('button')).toHaveAttribute('aria-expanded', 'true')
    await page.keyboard.press('Escape')
    await expect(panel).not.toBeVisible()
    if (destination === 'connection') {
      await expect(page).toHaveURL(/view=connection/)
      await expect(page.getByRole('link', { name: 'Connection settings', exact: true })).toBeFocused()
    } else if (destination === 'providers') {
      await expect(page).toHaveURL(/\/providers$/)
      await expect(
        page.getByRole('button', { name: 'Change history', exact: true }).filter({ visible: true }).first(),
      ).toBeFocused()
    } else {
      await expect(page).toHaveURL(/\/settings$/)
      // Settings has an in-page group navigation, so the current group link is the nearest visible landmark.
      await expect(
        page.getByRole('navigation', { name: 'Settings sections' }).getByRole('link', { name: 'General' }),
      ).toBeFocused()
    }
  })
}
for (const locale of ['en-US', 'zh-CN']) {
  for (const theme of ['light', 'dark']) {
    test(`exact toast reveal survives filter races and newer history (${locale}, ${theme})`, async ({ page }) => {
      await prepareApp(page)
      await page.addInitScript(
        ({ locale, theme }) => {
          localStorage.setItem('stravia-locale', locale)
          localStorage.setItem('stravia-theme', theme)
        },
        { locale, theme },
      )
      await page.setViewportSize(theme === 'dark' ? { width: 320, height: 740 } : { width: 1280, height: 800 })
      await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
      await page.route('**/api/v1/providers/history-provider/models', (route) =>
        route.fulfill({ json: { data: { models: [] } } }),
      )
      await page.route('**/api/v1/providers/history-provider/models/sync-state', (route) =>
        route.fulfill({
          json: {
            data: {
              automatic_sync: true,
              in_progress: false,
              pause_reason: null,
              last_check: summary,
              has_successful_sync: true,
            },
          },
        }),
      )
      await page.route('**/api/v1/providers/history-provider/models/sync', (route) =>
        route.fulfill({ json: { data: summary } }),
      )
      let release!: () => void
      const gate = new Promise<void>((done) => {
        release = done
      })
      let delayed = false
      await page.route('**/api/v1/model-changes?*', async (route) => {
        const q = new URL(route.request().url()).searchParams.get('q')
        if (q === 'slow') {
          delayed = true
          await gate
        }
        await route.fulfill({
          json: {
            data: {
              records: q ? [] : [{ ...exact, id: 'newer-other', provider_name: 'Newer Service' }],
              next_cursor: null,
            },
          },
        })
      })
      await page.route('**/api/v1/model-changes/exact-result', (route) => route.fulfill({ json: { data: exact } }))
      await page.goto('/providers/history-provider?view=models')
      await page
        .getByRole('button', { name: locale === 'en-US' ? 'Sync models' : '同步模型', exact: true })
        .first()
        .click()
      const toast = page
        .locator('[data-sonner-toast]')
        .filter({ hasText: locale === 'en-US' ? 'Model changes saved' : '模型变更已保存' })
      await expect(toast).toContainText(locale === 'en-US' ? 'Specification updated 1' : '规格更新 1')
      await page.getByRole('button', { name: locale === 'en-US' ? 'Change history' : '变更记录', exact: true }).click()
      const panel = page.getByRole('dialog')
      await panel.locator('#history-search').fill('slow')
      await expect.poll(() => delayed).toBe(true)
      // Toast lives outside the modal; escape returns to the original operation.
      await page.keyboard.press('Escape')
      await toast.getByRole('button', { name: locale === 'en-US' ? 'View details' : '查看详情' }).click()
      release()
      const selected = panel.getByLabel(locale === 'en-US' ? 'View details' : '查看详情', { exact: true })
      await expect(selected).toContainText('Exact Model')
      await expect(selected).toContainText('exact-model')
      await expect(selected).toContainText('[]')
      await expect(selected).toContainText(locale === 'en-US' ? 'After: 0' : '之后: 0')
      await expect(selected).not.toContainText('Newer Service')
      await expect(selected.getByRole('button')).toHaveAttribute('aria-expanded', 'true')
    })
  }
}
test('320px history keeps error, empty and no-match distinct and restores keyboard focus', async ({ page }) => {
  await prepareApp(page)
  await page.setViewportSize({ width: 320, height: 740 })
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
  let fail = true
  await page.route('**/api/v1/model-changes?*', (route) =>
    route.fulfill(
      fail
        ? { status: 503, json: { error: 'History temporarily unavailable' } }
        : { json: { data: { records: [], next_cursor: null } } },
    ),
  )
  await page.goto('/providers')
  const trigger = page.getByRole('button', { name: 'Change history', exact: true }).filter({ visible: true })
  await trigger.focus()
  await page.keyboard.press('Enter')
  const panel = page.getByRole('dialog')
  await expect(panel).toContainText('History temporarily unavailable')
  await expect(panel).not.toContainText('No model changes recorded yet.')
  fail = false
  await panel.getByRole('button', { name: /Retry|Try again/ }).click()
  await expect(panel).toContainText('No model changes recorded yet.')
  await panel.locator('#history-search').fill('absent')
  await expect(panel).toContainText('No records match these filters.')
  expect(await panel.evaluate((element) => element.getBoundingClientRect().width)).toBeLessThanOrEqual(320)
  await page.keyboard.press('Escape')
  await expect(panel).not.toBeVisible()
  await expect(trigger).toBeFocused()
})

test('manual no-change, saved partial result and saved failure expose truthful recovery', async ({ page }) => {
  await prepareApp(page)
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
  await page.route('**/api/v1/providers/history-provider/models', (route) =>
    route.fulfill({ json: { data: { models: [] } } }),
  )
  let result: ProviderModelSyncSummary = {
    ...summary,
    record_id: null,
    deprecation_cleared: 0,
    specification_updated: 0,
  }
  await page.route('**/api/v1/providers/history-provider/models/sync-state', (route) =>
    route.fulfill({
      json: {
        data: {
          automatic_sync: true,
          in_progress: false,
          pause_reason: null,
          last_check: result,
          has_successful_sync: true,
        },
      },
    }),
  )
  await page.route('**/api/v1/providers/history-provider/models/sync', (route) =>
    route.fulfill(
      result.outcome === 'failure'
        ? { status: 502, json: { error: result.error, details: { sync_summary: result } } }
        : { json: { data: result } },
    ),
  )
  await page.route('**/api/v1/model-changes?*', (route) =>
    route.fulfill({ json: { data: { records: [], next_cursor: null } } }),
  )
  await page.route('**/api/v1/model-changes/exact-result', (route) =>
    route.fulfill({ json: { data: { ...exact, outcome: result.outcome, error: result.error } } }),
  )
  await page.goto('/providers/history-provider?view=models')
  await expect(page.getByRole('switch', { name: 'Automatic sync' })).toBeChecked()
  await expect(page.locator('[data-sonner-toast]')).toHaveCount(0)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  const sync = page.getByRole('button', { name: 'Sync models', exact: true }).first()
  await sync.click()
  const unchanged = page.locator('[data-sonner-toast]').filter({ hasText: 'Check complete, model inventory unchanged' })
  await expect(unchanged).toBeVisible()
  await expect(unchanged.getByRole('button', { name: 'View details' })).toHaveCount(0)
  result = { ...summary, outcome: 'partial_success', error: 'Saved inventory; credential recovery unavailable' }
  await sync.click()
  const partial = page
    .locator('[data-sonner-toast]')
    .filter({ hasText: 'Model changes saved, but a later step failed' })
  await expect(partial).toContainText('Specification updated 1')
  await expect(partial).toContainText('credential recovery unavailable')
  await expect(partial.getByRole('button', { name: 'Retry' })).toBeVisible()
  result = { ...result, outcome: 'failure', error: 'Discovery unavailable; saved inventory retained' }
  await partial.getByRole('button', { name: 'Retry' }).click()
  const failure = page.locator('[data-sonner-toast]').filter({ hasText: 'Model sync failed' })
  await expect(failure).toContainText('saved inventory retained')
  await failure.getByRole('button', { name: 'View details' }).click()
  await expect(page.getByRole('dialog').getByLabel('View details', { exact: true })).toContainText(
    'Discovery unavailable',
  )
})

test('older keyset pages remain whole batches when a newer record arrives', async ({ page }) => {
  await prepareApp(page)
  await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
  const older = {
    ...exact,
    id: 'older',
    provider_name: 'Older batch',
    changes: [
      ...exact.changes,
      { ...exact.changes[0], model_id: 'other-in-batch', name: 'Other model in matching batch' },
    ],
  }
  let newer = false
  await page.route('**/api/v1/model-changes?*', (route) => {
    const params = new URL(route.request().url()).searchParams
    const records =
      params.get('cursor') === 'opaque-before-exact'
        ? [older]
        : newer
          ? [{ ...exact, id: 'newest', provider_name: 'Newest batch' }, exact]
          : [exact]
    return route.fulfill({
      json: { data: { records, next_cursor: params.has('cursor') ? null : 'opaque-before-exact' } },
    })
  })
  await page.goto('/providers')
  await page.getByRole('button', { name: 'Change history', exact: true }).filter({ visible: true }).click()
  const panel = page.getByRole('dialog')
  await panel.locator('#history-search').fill('exact-model')
  newer = true
  await panel.getByRole('button', { name: 'Load older records' }).click()
  const olderBatch = panel.getByRole('button').filter({ hasText: 'Older batch' })
  await olderBatch.focus()
  await page.keyboard.press('Space')
  await expect(panel).toContainText('Other model in matching batch')
  await expect(panel.getByRole('button').filter({ hasText: 'History Service' })).toHaveCount(1)
  await expect(olderBatch).toHaveCount(1)
  await expect(panel.getByRole('button', { name: 'Load older records' })).toHaveCount(0)
})

for (const locale of ['en-US', 'zh-CN'] as const) {
  test(`upstream model toolbar keeps the same two rows with every state present (${locale})`, async ({ page }) => {
    await prepareApp(page)
    await page.addInitScript((value) => localStorage.setItem('stravia-locale', value), locale)
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.route('**/api/v1/providers', (route) => route.fulfill({ json: { data: [provider] } }))
    await page.route('**/api/v1/providers/*/models', (route) => route.fulfill({ json: { data: { models: [] } } }))
    await page.route('**/api/v1/providers/*/models/sync-state', (route) =>
      route.fulfill({
        json: {
          data: {
            automatic_sync: true,
            in_progress: true,
            pause_reason: 'Provider credentials are invalid',
            last_check: summary,
            has_successful_sync: true,
          },
        },
      }),
    )
    await page.goto(`/providers/${provider.id}?view=models`)
    const toolbar = page.getByTestId('model-inventory-toolbar')
    const status = toolbar.locator('[data-toolbar-row="status"]')
    await expect(status).toBeVisible()
    await expect(toolbar.locator('[data-toolbar-row]')).toHaveCount(2)

    // 行数 = 行高 / 行高度；同一行的子项共享同一条基线区间，不因语言换到另一行。
    const rows = await toolbar.locator('[data-toolbar-row]').evaluateAll((elements) =>
      elements.map((element) => {
        const lineHeight = (node: Element) => Number.parseFloat(getComputedStyle(node).lineHeight) || 20
        const box = element.getBoundingClientRect()
        const tops = new Set(
          Array.from(element.querySelectorAll(':scope > *, :scope > * > *'))
            .filter((child) => child.getBoundingClientRect().height > 0)
            .map((child) => Math.round(child.getBoundingClientRect().top / 8)),
        )
        return {
          kind: element.getAttribute('data-toolbar-row'),
          lines: Math.round(box.height / Math.max(lineHeight(element), 36)),
          tops: tops.size,
          bottom: box.bottom,
        }
      }),
    )
    expect(rows.map((row) => row.kind)).toEqual(['actions', 'status'])
    // 第一行：自动同步、变更记录、手动添加与同步在一条线上；第二行：结果与状态同样各自一行。
    expect(rows[0].lines).toBe(1)
    expect(rows[1].lines).toBe(1)
    const actionButtons = toolbar.locator('[data-toolbar-row="actions"] button:not([role="switch"])')
    const tops = await actionButtons.evaluateAll((buttons) =>
      buttons.map((button) => Math.round(button.getBoundingClientRect().top)),
    )
    expect(new Set(tops).size).toBe(1)
    await expect(page.locator('[data-slot="table-container"]').first()).toBeVisible()
  })
}
