import { expect, test } from '@playwright/test'
import type { Locator, Page, Route } from '@playwright/test'

import type { CredentialRule, CustomCredentialRule, CustomCredentialRuleInput } from '../src/lib/types'
import { prepareApp } from './prepare-app'

function builtin(index: number): CredentialRule {
  return {
    id: `fixture-rule-${index}`,
    name: `Fixture credential ${index}`,
    target: 'Fixture API credentials',
    description: 'Detects test-only API credentials with local context.',
    regex: 'test_[A-Za-z0-9]+',
    path: null,
    secret_group: 0,
    keywords: ['test_'],
    filter: '',
    components: [],
    skip_report: false,
    specificity: 10,
    confidence: 'high',
  }
}

const savedRule: CustomCredentialRule = {
  id: 'custom-1',
  name: 'Internal token',
  description: '',
  enabled: true,
  spec: { mode: 'simple', text: 'internal-secret-value' },
  created_at: 1,
  updated_at: 1,
}

interface Mock {
  customRules: CustomCredentialRule[]
  writes: Array<{ method: string; path: string; body: unknown }>
  failNextWrite: { status: number; json: unknown } | null
  testText: string[]
  failTest: boolean
}

async function mockRedaction(page: Page, options: { rules?: number; custom?: CustomCredentialRule[] } = {}) {
  const state: Mock = {
    customRules: [...(options.custom ?? [])],
    writes: [],
    failNextWrite: null,
    testText: [],
    failTest: false,
  }
  const rules = Array.from({ length: options.rules ?? 35 }, (_, index) => builtin(index))
  await page.route('**/api/v1/settings/reversible_redaction_enabled', (route) =>
    route.fulfill({ json: { data: 'false' } }),
  )
  await page.route('**/api/v1/reversible-redaction/**', async (route: Route) => {
    const request = route.request()
    const { pathname } = new URL(request.url())
    const method = request.method()
    if (pathname.endsWith('/rules')) return route.fulfill({ json: { data: { rules, prefilter: '', filter: '' } } })
    if (pathname.endsWith('/discoveries')) {
      return route.fulfill({ json: { data: { items: [], next_cursor: null, observation_gap: false } } })
    }
    if (pathname.endsWith('/test')) {
      const { text } = request.postDataJSON() as { text: string }
      state.testText.push(text)
      if (state.failTest) return route.fulfill({ status: 503, json: { error: 'Fixture detector failure' } })
      const start = text.indexOf('test_credential')
      return route.fulfill({
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
                      start_line: 1,
                      start_column: start + 1,
                      end_line: 1,
                      end_column: start + 16,
                    },
                  ],
          },
        },
      })
    }
    if (method === 'GET') return route.fulfill({ json: { data: state.customRules } })
    state.writes.push({ method, path: pathname, body: request.postDataJSON() })
    if (state.failNextWrite) {
      const failure = state.failNextWrite
      state.failNextWrite = null
      return route.fulfill(failure)
    }
    if (method === 'DELETE') {
      state.customRules = state.customRules.filter((rule) => !pathname.endsWith(`/${rule.id}`))
      return route.fulfill({ json: { data: null } })
    }
    const input = request.postDataJSON() as CustomCredentialRuleInput
    const id = method === 'PUT' ? (pathname.split('/').pop() as string) : `custom-${state.customRules.length + 1}`
    const rule: CustomCredentialRule = { ...input, id, created_at: 1, updated_at: 2 }
    state.customRules =
      method === 'PUT' ? state.customRules.map((r) => (r.id === id ? rule : r)) : [...state.customRules, rule]
    return route.fulfill({ json: { data: rule } })
  })
  return state
}

function overlaps(a: { x: number; y: number; width: number; height: number }, b: typeof a): boolean {
  return (
    a.x < b.x + b.width - 0.5 && b.x < a.x + a.width - 0.5 && a.y < b.y + b.height - 0.5 && b.y < a.y + a.height - 0.5
  )
}

async function box(locator: Locator) {
  const result = await locator.boundingBox()
  if (!result) throw new Error('Element is not visible')
  return result
}

test.beforeEach(async ({ page }) => {
  await prepareApp(page)
})

test.describe('rules workspace', () => {
  test('fills the 120rem container and the remaining height at 2560px with content-sized columns', async ({ page }) => {
    await mockRedaction(page)
    await page.setViewportSize({ width: 2560, height: 720 })
    await page.goto('/reversible-redaction')
    const table = page.getByRole('table')
    await expect(table.getByRole('button', { name: /Fixture credential 0/ })).toBeVisible()
    const title = await box(page.locator('main h1').last())
    const tableArea = await box(page.locator('[data-slot="data-table"]'))
    // 容器上限 120rem = 1920px，表格与页头共用左缘并填满容器。
    expect(Math.abs(tableArea.x - title.x)).toBeLessThan(2)
    expect(tableArea.width).toBeGreaterThan(1900)
    expect(tableArea.width).toBeLessThanOrEqual(1921)
    // 名称列按内容定宽，条件列吸收其余宽度：不会把两列均匀拉开。
    const nameCell = await box(table.getByRole('columnheader').nth(0))
    const conditionCell = await box(table.getByRole('columnheader').nth(1))
    expect(nameCell.width).toBeLessThan(500)
    expect(conditionCell.width).toBeGreaterThan(nameCell.width * 2)
    // 分页器贴在工作区底部，没有固定限高造成的大块留白。
    const main = await box(page.getByRole('main'))
    const paginator = await box(page.locator('[data-slot="data-table-paginator"]'))
    const gap = main.y + main.height - paginator.y - paginator.height
    expect(gap).toBeGreaterThanOrEqual(0)
    expect(gap).toBeLessThanOrEqual(24)
  })

  for (const width of [390, 320]) {
    test(`keeps search, filter, tabs and paginator apart and operable at ${width}px`, async ({ page }) => {
      await mockRedaction(page)
      await page.setViewportSize({ width, height: 800 })
      await page.goto('/reversible-redaction')
      const toolbar = page.locator('[data-slot="data-table-toolbar"]')
      await expect(toolbar).toBeVisible()
      const tabs = await box(page.getByRole('tablist'))
      const search = await box(toolbar.locator('[data-slot="input-group"]').first())
      const filter = await box(toolbar.getByLabel('Rule source'))
      expect(search.y).toBeGreaterThanOrEqual(tabs.y + tabs.height - 0.5)
      expect(filter.y).toBeGreaterThanOrEqual(search.y + search.height - 0.5)
      expect(overlaps(search, filter)).toBe(false)
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)

      const next = page.getByRole('button', { name: 'Next', exact: true })
      await next.scrollIntoViewIfNeeded()
      await expect(next).toBeInViewport()
      await next.click()
      await expect(page.getByText('Page 2 of 4')).toBeVisible()
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
    })
  }

  test('keeps a valid page after filtering and searching from the last page', async ({ page }) => {
    await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await page.getByRole('button', { name: 'Last page', exact: true }).click()
    await expect(page.getByText('Page 4 of 4')).toBeVisible()
    await page.locator('#credential-rule-search').fill('Fixture credential 3')
    // Fixture credential 3、30–34 共 6 条，回到有效的第 1 页并显示数据。
    await expect(page.getByText('Page 1 of 1')).toBeVisible()
    await expect(page.getByRole('button', { name: /Fixture credential 34/ })).toBeVisible()
    await page.locator('#credential-rule-search').fill('')
    await page.getByRole('button', { name: 'Last page', exact: true }).click()
    await expect(page.getByText(/Page 4 of 4/)).toBeVisible()
    await page.getByLabel('Rule source').click()
    await page.getByRole('option', { name: 'Custom rules' }).click()
    await expect(page.getByText('Page 1 of 1')).toBeVisible()
    await expect(page.getByRole('button', { name: /Internal token/ })).toBeVisible()
  })

  test('keeps the paginator reachable when the window is low', async ({ page }) => {
    await mockRedaction(page)
    await page.setViewportSize({ width: 1280, height: 420 })
    await page.goto('/reversible-redaction')
    await expect(page.getByRole('table').getByRole('button', { name: /Fixture credential 0/ })).toBeVisible()
    const next = page.getByRole('button', { name: 'Next', exact: true })
    await next.scrollIntoViewIfNeeded()
    await expect(next).toBeInViewport()
    // 低窗口保留至少 24rem 工作区并允许页面滚动。
    const workspace = await box(page.locator('[data-slot="data-table"]'))
    expect(workspace.height).toBeGreaterThanOrEqual(24 * 16 - 2)
    await next.click()
    await expect(page.getByText('Page 2 of 4')).toBeVisible()
    // 滚到底后，分页器下方仍保留主内容区的正常底部内边距。
    await page.getByRole('main').evaluate((element) => element.scrollTo(0, element.scrollHeight))
    const main = await box(page.getByRole('main'))
    const paginator = await box(page.locator('[data-slot="data-table-paginator"]'))
    expect(main.y + main.height - paginator.y - paginator.height).toBeGreaterThanOrEqual(12)
  })
})

test.describe('matching test', () => {
  test('puts input and results side by side when the workspace is wide and stacks them in order when narrow', async ({
    page,
  }) => {
    await mockRedaction(page)
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await page.getByRole('tab', { name: 'Matching test', exact: true }).click()
    const input = page.getByRole('textbox', { name: 'Text to test' })
    const results = page.getByRole('region', { name: 'Match results' })
    const wideInput = await box(input)
    const wideResults = await box(results)
    expect(wideResults.x).toBeGreaterThanOrEqual(wideInput.x + wideInput.width)
    expect(Math.abs(wideResults.y - wideInput.y)).toBeLessThan(wideInput.height)

    await page.setViewportSize({ width: 390, height: 800 })
    const action = await box(page.getByRole('button', { name: 'Test matching', exact: true }))
    const flow = page.getByText('Test text is sent only to this Stravia instance, not to model providers.')
    await expect(flow).toBeVisible()
    const narrowInput = await box(input)
    const narrowResults = await box(results)
    expect(action.y).toBeGreaterThanOrEqual(narrowInput.y + narrowInput.height - 0.5)
    expect(narrowResults.y).toBeGreaterThanOrEqual(action.y + action.height - 0.5)
    expect(narrowResults.x).toBeLessThan(narrowInput.x + narrowInput.width)
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
  })

  test('runs a test, reaches the stacked result, locates the match and tells failure from no match', async ({
    page,
  }) => {
    const state = await mockRedaction(page)
    await page.setViewportSize({ width: 390, height: 700 })
    await page.goto('/reversible-redaction')
    await page.getByRole('tab', { name: 'Matching test', exact: true }).click()
    const input = page.getByRole('textbox', { name: 'Text to test' })
    const text = '说明😀 test_credential 后文'
    await input.fill(text)
    expect(state.testText).toEqual([])
    await page.getByRole('button', { name: 'Test matching', exact: true }).click()
    const status = page.getByRole('status').filter({ hasText: '1 matches' })
    await expect(status).toBeVisible()
    // 结果堆叠在操作之后，运行结束后落入视口。
    await expect(status).toBeInViewport()
    const match = page.getByRole('button', { name: /Fixture credential 0/ })
    await expect(match).toBeInViewport()
    await match.click()
    expect(
      await input.evaluate((element: HTMLTextAreaElement) =>
        element.value.slice(element.selectionStart, element.selectionEnd),
      ),
    ).toBe('test_credential')

    await input.fill('ordinary text')
    await page.getByRole('button', { name: 'Test matching', exact: true }).click()
    await expect(page.getByRole('status').filter({ hasText: 'No existing rule matched' })).toBeInViewport()
    await expect(page.getByRole('alert')).toHaveCount(0)

    state.failTest = true
    await page.getByRole('button', { name: 'Test matching', exact: true }).click()
    await expect(page.getByRole('alert').filter({ hasText: 'Matching test failed.' })).toBeInViewport()
    await expect(page.getByRole('status').filter({ hasText: 'No existing rule matched' })).toHaveCount(0)
    expect(state.testText).toEqual([text, 'ordinary text', 'ordinary text'])
  })
})

test.describe('custom rule Sheet', () => {
  const sheetOf = (page: Page) => page.locator('[data-slot="sheet-content"]')
  const discard = (page: Page) => page.getByRole('alertdialog', { name: 'Discard unsaved changes?' })

  test('keeps the enabled switch as an unsaved draft until the rule is saved', async ({ page }) => {
    const state = await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/reversible-redaction')
    await page.getByRole('button', { name: 'Internal token' }).click()
    const sheet = sheetOf(page)
    const enabled = sheet.getByRole('switch', { name: 'Enable this rule' })
    await expect(enabled).toBeChecked()
    await enabled.click()
    await expect(enabled).not.toBeChecked()
    expect(state.writes).toEqual([])
    await page.keyboard.press('Escape')
    await expect(discard(page)).toBeVisible()
    await discard(page).getByRole('button', { name: 'Keep editing' }).click()
    await expect(enabled).not.toBeChecked()
    expect(state.writes).toEqual([])

    await sheet.getByRole('button', { name: 'Save rule' }).click()
    await expect(sheet).toBeHidden()
    expect(state.writes).toHaveLength(1)
    expect(state.writes[0]).toMatchObject({
      method: 'PUT',
      path: '/api/v1/reversible-redaction/custom-rules/custom-1',
      body: { enabled: false, spec: { mode: 'simple', text: 'internal-secret-value' } },
    })
    await expect(page.getByRole('row').filter({ hasText: 'Internal token' })).toContainText('Disabled')
  })

  test('explains optional fields only through question-mark hints', async ({ page }) => {
    await mockRedaction(page)
    await page.setViewportSize({ width: 1280, height: 900 })
    await page.goto('/reversible-redaction')
    await page.getByRole('button', { name: 'Add custom rule' }).click()
    const sheet = sheetOf(page)
    await expect(sheet.getByRole('heading', { name: 'Add custom rule' })).toBeVisible()
    // 没有常驻说明文字：标题之外既无 Sheet 描述也无字段说明。
    await expect(sheet.locator('[data-slot="sheet-description"]')).toHaveCount(0)
    await expect(sheet.locator('[data-slot="field-description"]')).toHaveCount(0)
    await sheet.getByRole('button', { name: 'More about this field' }).first().focus()
    await expect(page.locator('[data-slot="tooltip-content"]')).toContainText('Case-sensitive and matched as typed')
    await page.keyboard.press('Escape')
    await sheet.getByRole('tab', { name: 'Rule' }).click()
    const regexLabel = sheet.getByText('Matching expression', { exact: true })
    await expect(regexLabel).toBeVisible()
    // 规则模式的字段不再把说明推下首屏：备注与启用开关在 900px 高度内可见。
    await expect(sheet.getByRole('switch', { name: 'Enable this rule' })).toBeInViewport()
  })

  test('creates a rule, keeps the typed secret after a failed save, and re-masks it on every open', async ({
    page,
  }) => {
    const state = await mockRedaction(page)
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/reversible-redaction')
    await page.getByRole('button', { name: 'Add custom rule' }).click()
    const sheet = sheetOf(page)
    await sheet.getByLabel('Rule name').fill('Staging token')
    const secret = sheet.getByLabel('Text to protect')
    await sheet.getByRole('button', { name: 'Show secret' }).click()
    await secret.fill('staging-secret-xyz')
    await expect(secret).toHaveAttribute('data-revealed', 'true')

    state.failNextWrite = { status: 503, json: { error: 'Fixture save unavailable' } }
    await sheet.getByRole('button', { name: 'Add rule' }).click()
    await expect(sheet.getByRole('alert').filter({ hasText: 'Fixture save unavailable' })).toBeVisible()
    await expect(sheet).toBeVisible()
    await expect(secret).toHaveValue('staging-secret-xyz')
    await expect(sheet.getByLabel('Rule name')).toHaveValue('Staging token')

    // 字段级错误留在对应输入下，失败输入仍在。
    state.failNextWrite = {
      status: 422,
      json: { error: 'invalid', code: 'custom_credential_rule_invalid', params: { field: 'text', reason: 'too_long' } },
    }
    await sheet.getByRole('button', { name: 'Add rule' }).click()
    await expect(sheet.getByRole('alert').filter({ hasText: 'This value is too long.' })).toBeVisible()
    await expect(secret).toHaveValue('staging-secret-xyz')

    await sheet.getByRole('button', { name: 'Add rule' }).click()
    await expect(sheet).toBeHidden()
    expect(state.writes.at(-1)).toMatchObject({
      method: 'POST',
      body: { name: 'Staging token', enabled: true, spec: { mode: 'simple', text: 'staging-secret-xyz' } },
    })
    // 逐字文本不进入表格。
    await expect(page.getByRole('button', { name: /Staging token/ })).toBeVisible()
    await expect(page.getByText('staging-secret-xyz')).toHaveCount(0)

    await page.getByRole('button', { name: /Staging token/ }).click()
    const reopened = sheet.getByLabel('Text to protect')
    await expect(reopened).toHaveAttribute('data-revealed', 'false')
    await expect(sheet.getByRole('button', { name: 'Show secret' })).toHaveAttribute('aria-pressed', 'false')
    await sheet.getByRole('button', { name: 'Show secret' }).click()
    await expect(reopened).toHaveAttribute('data-revealed', 'true')
    await sheet.getByRole('button', { name: 'Cancel' }).click()
    await expect(sheet).toBeHidden()
    await page.getByRole('button', { name: /Staging token/ }).click()
    await expect(sheet.getByLabel('Text to protect')).toHaveAttribute('data-revealed', 'false')
  })

  test('keeps the editor when a delete fails and removes the named rule when it succeeds', async ({ page }) => {
    const state = await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/reversible-redaction')
    await page.getByRole('button', { name: 'Internal token' }).click()
    const sheet = sheetOf(page)
    await sheet.getByRole('button', { name: 'Delete rule' }).click()
    const confirm = page.getByRole('alertdialog', { name: 'Delete “Internal token”?' })
    await expect(confirm).toBeVisible()
    state.failNextWrite = { status: 503, json: { error: 'Fixture delete unavailable' } }
    await confirm.getByRole('button', { name: 'Delete rule' }).click()
    await expect(sheet.getByRole('alert').filter({ hasText: 'Fixture delete unavailable' })).toBeVisible()
    await expect(sheet).toBeVisible()
    await expect(sheet.getByLabel('Rule name')).toHaveValue('Internal token')

    await sheet.getByRole('button', { name: 'Delete rule' }).click()
    await page
      .getByRole('alertdialog', { name: 'Delete “Internal token”?' })
      .getByRole('button', { name: 'Delete rule' })
      .click()
    await expect(sheet).toBeHidden()
    await expect(page.getByRole('button', { name: 'Internal token' })).toHaveCount(0)
    expect(state.writes.map((write) => write.method)).toEqual(['DELETE', 'DELETE'])
  })

  test('asks before leaving the page with an unsaved draft', async ({ page }) => {
    const state = await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.route('**/api/v1/providers**', (route) => route.fulfill({ json: { data: [] } }))
    await page.goto('/reversible-redaction')
    await page.getByRole('button', { name: 'Internal token' }).click()
    const sheet = sheetOf(page)
    await sheet.getByLabel('Rule name').fill('Renamed token')
    await page.evaluate(() => document.querySelector<HTMLAnchorElement>('a[href="/providers"]')?.click())
    await expect(discard(page)).toBeVisible()
    await discard(page).getByRole('button', { name: 'Keep editing' }).click()
    await expect(page).toHaveURL(/\/reversible-redaction$/)
    await expect(sheet.getByLabel('Rule name')).toHaveValue('Renamed token')
    expect(state.writes).toEqual([])
  })
})
