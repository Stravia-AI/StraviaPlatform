import { expect, test } from '@playwright/test'
import type { Locator, Page, Route } from '@playwright/test'

import type {
  CredentialDiscoveryRecord,
  CredentialHitDetail,
  CredentialHitReuse,
  CredentialHitReuses,
  CredentialRule,
  CustomCredentialRule,
  CustomCredentialRuleInput,
} from '../src/lib/types'
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

const createdAt = 1_788_854_400_000

const entryLines = [
  'first line',
  'second line',
  'third line',
  'fourth line',
  'secret test_credential here',
  'sixth line',
  'seventh line other_secret here',
  'eighth line',
  'ninth line',
  'tenth line',
]
const entryText = entryLines.join('\n')
const secretStart = entryText.indexOf('test_credential')
const otherStart = entryText.indexOf('other_secret')

interface DiscoveryQuery {
  cursor: string | null
  startAt: number | null
  endAt: number | null
}

type DiscoveryReply = { items: CredentialDiscoveryRecord[]; next_cursor?: string | null } | { status: number }
type DetailReply = CredentialHitDetail | { status: number }
type ReusesReply = CredentialHitReuses | { status: number }

interface Mock {
  customRules: CustomCredentialRule[]
  writes: Array<{ method: string; path: string; body: unknown }>
  failNextWrite: { status: number; json: unknown } | null
  testText: string[]
  failTest: boolean
  discoveryRequests: DiscoveryQuery[]
  detailRequests: Array<{ id: string; index: number }>
  reuseRequests: string[]
}

interface MockOptions {
  rules?: number
  custom?: CustomCredentialRule[]
  discoveries?: CredentialDiscoveryRecord[]
  /** 按请求顺序给出每一页；`call` 从 1 开始，便于模拟某一次请求失败。 */
  discoveryPage?: (query: DiscoveryQuery, call: number) => DiscoveryReply
  detail?: (id: string, index: number) => DetailReply
  reuses?: (id: string) => ReusesReply
}

function discovery(index: number, overrides: Partial<CredentialDiscoveryRecord> = {}): CredentialDiscoveryRecord {
  return {
    id: `100.${index}`,
    created_at: createdAt - index * 1000,
    interaction_id: `interaction-${index}`,
    run_id: `run-${index}`,
    api_key_name: 'omp-key',
    rule_sets: ['builtin'],
    rule_ids: ['fixture-rule-0', 'fixture-rule-1'],
    hits: [
      { rule_id: 'fixture-rule-0', source_type: 'tool_result' },
      { rule_id: 'fixture-rule-1', source_type: 'tool_result' },
    ],
    omitted_hits: 0,
    linkable: true,
    ...overrides,
  }
}

function availableDetail(
  overrides: Partial<Extract<CredentialHitDetail, { state: 'available' }>> = {},
): CredentialHitDetail {
  return {
    state: 'available',
    field: { source_type: 'tool_result', tool_id: 'tool-1', tool_name: 'Lookup' },
    text: entryText,
    truncated: false,
    hit_basis: 'secret',
    matches: [
      {
        rule_id: 'fixture-rule-0',
        start: secretStart,
        end: secretStart + 'test_credential'.length,
        start_line: 5,
        start_column: 8,
        end_line: 5,
        end_column: 23,
        hit: true,
      },
      {
        rule_id: 'fixture-rule-1',
        start: otherStart,
        end: otherStart + 'other_secret'.length,
        start_line: 7,
        start_column: 14,
        end_line: 7,
        end_column: 26,
        hit: false,
      },
    ],
    ...overrides,
  }
}

function reuse(overrides: Partial<CredentialHitReuse> = {}): CredentialHitReuse {
  return {
    interaction_id: 'reuse-1',
    run_id: 'run-reuse-1',
    api_key_name: 'omp-key',
    source_type: 'tool_arguments',
    tool_id: 'tool-9',
    latest_input: null,
    last_reused_at: createdAt,
    ...overrides,
  }
}

/** 按 30 条一页切分 `total` 条命中记录，游标是下一页的起始下标。 */
function pagedDiscoveries(total: number) {
  return ({ cursor }: DiscoveryQuery): DiscoveryReply => {
    const offset = cursor ? Number(cursor) : 0
    const count = Math.min(30, total - offset)
    const next = offset + count
    return {
      items: Array.from({ length: count }, (_, index) =>
        discovery(offset + index, { api_key_name: `key-${offset + index}` }),
      ),
      next_cursor: next < total ? String(next) : null,
    }
  }
}

async function openRules(page: Page) {
  await page.getByRole('tab', { name: /Current rules/ }).click()
}

async function mockRedaction(page: Page, options: MockOptions = {}) {
  const state: Mock = {
    customRules: [...(options.custom ?? [])],
    writes: [],
    failNextWrite: null,
    testText: [],
    failTest: false,
    discoveryRequests: [],
    detailRequests: [],
    reuseRequests: [],
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
      const params = new URL(request.url()).searchParams
      const bound = (name: string) => (params.has(name) ? Number(params.get(name)) : null)
      const query = { cursor: params.get('cursor'), startAt: bound('start_at'), endAt: bound('end_at') }
      state.discoveryRequests.push(query)
      const reply = options.discoveryPage?.(query, state.discoveryRequests.length) ?? {
        items: query.cursor ? [] : (options.discoveries ?? []),
      }
      if ('status' in reply) {
        return route.fulfill({ status: reply.status, json: { error: 'Fixture hit records unavailable' } })
      }
      return route.fulfill({ json: { data: { items: reply.items, next_cursor: reply.next_cursor ?? null } } })
    }
    const hitPath = pathname.match(/\/reversible-redaction\/discoveries\/([^/]+)\/hits\/(\d+)$/)
    if (hitPath) {
      const id = decodeURIComponent(hitPath[1])
      const index = Number(hitPath[2])
      state.detailRequests.push({ id, index })
      const reply = options.detail?.(id, index) ?? availableDetail()
      if ('status' in reply) return route.fulfill({ status: reply.status, json: { error: 'Fixture hit unavailable' } })
      return route.fulfill({ json: { data: reply } })
    }
    const reusePath = pathname.match(/\/reversible-redaction\/discoveries\/([^/]+)\/reuses$/)
    if (reusePath) {
      const id = decodeURIComponent(reusePath[1])
      state.reuseRequests.push(id)
      const reply = options.reuses?.(id) ?? { linkable: true, items: [], truncated: false }
      if ('status' in reply)
        return route.fulfill({ status: reply.status, json: { error: 'Fixture reuse unavailable' } })
      return route.fulfill({ json: { data: reply } })
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

const rowOf = (page: Page, apiKey: string) =>
  page.getByRole('row').filter({ has: page.getByRole('cell', { name: apiKey, exact: true }) })

const chip = (scope: Locator, rule: string, source: string) =>
  scope.getByRole('button', { name: `${rule} found in ${source}`, exact: true })

const hitViewport = (page: Page) => page.locator('[data-slot="hit-records-viewport"]')

async function scrollHits(page: Page, to: 'top' | 'bottom' | number) {
  await hitViewport(page).evaluate((element, target) => {
    element.scrollTop = target === 'top' ? 0 : target === 'bottom' ? element.scrollHeight : target
  }, to)
}

async function openRow(page: Page, apiKey = 'omp-key') {
  await rowOf(page, apiKey).getByRole('cell').nth(1).click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toBeVisible()
  return dialog
}

test.describe('hit records', () => {
  test('is the first and default tab and shows one created mapping per row', async ({ page }) => {
    await mockRedaction(page, { discoveries: [discovery(0)] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const tabs = page.getByRole('tab')
    await expect(tabs.nth(0)).toHaveText(/Hit records/)
    await expect(tabs.nth(1)).toHaveText(/Current rules/)
    await expect(tabs.nth(2)).toHaveText(/Matching test/)
    await expect(tabs.nth(0)).toHaveAttribute('aria-selected', 'true')

    await expect(page.getByRole('columnheader')).toHaveCount(5)
    const row = rowOf(page, 'omp-key')
    const cells = row.getByRole('cell')
    await expect(cells.nth(0)).toHaveText(/\d{4}/)
    await expect(cells.nth(1)).toHaveText('omp-key')
    await expect(cells.nth(2)).toContainText('Built-in')
    await expect(cells.nth(3)).toContainText('Fixture credential 0')
    await expect(cells.nth(3)).toContainText('Fixture credential 1')
    const hits = cells.nth(4)
    await expect(hits.getByRole('listitem')).toHaveCount(2)
    await expect(chip(hits, 'Fixture credential 0', 'Tool result')).toBeVisible()
    await expect(chip(hits, 'Fixture credential 1', 'Tool result')).toBeVisible()
    // 创建映射没有请求状态，也没有行内跳转箭头。
    await expect(row.getByRole('status')).toHaveCount(0)
    await expect(row.getByRole('link')).toHaveCount(0)
  })

  test('collapses extra hit pairs and adds the locators omitted during capture', async ({ page }) => {
    await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          rule_ids: ['fixture-rule-0'],
          hits: [
            { rule_id: 'fixture-rule-0', source_type: 'tool_result' },
            { rule_id: 'fixture-rule-0', source_type: 'tool_arguments' },
            { rule_id: 'fixture-rule-0', source_type: 'user_message', latest_input: true },
            { rule_id: 'fixture-rule-1', source_type: 'tool_result' },
            { rule_id: 'fixture-rule-1', source_type: 'user_message', latest_input: false },
          ],
          omitted_hits: 2,
        }),
      ],
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const hits = rowOf(page, 'omp-key').getByRole('cell').nth(4)
    await expect(hits.getByRole('listitem')).toHaveCount(4)
    await expect(hits.getByText('+4', { exact: true })).toBeVisible()
  })

  test('opens a side panel with field context, the full entry and match details', async ({ page }) => {
    await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          rule_ids: ['fixture-rule-0'],
          hits: [{ rule_id: 'fixture-rule-0', source_type: 'tool_result', tool_id: 'tool-1' }],
        }),
      ],
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const row = rowOf(page, 'omp-key')
    const dialog = await openRow(page)
    await expect(dialog.getByRole('heading', { name: /omp-key/ })).toBeVisible()
    await expect(dialog.getByRole('heading', { name: /2026/ })).toBeVisible()

    await expect(chip(dialog, 'Fixture credential 0', 'Tool result')).toHaveAttribute('aria-pressed', 'true')
    const fieldSection = dialog.locator('section').filter({ has: page.getByRole('heading', { name: 'Field context' }) })
    await expect(fieldSection).toContainText('Tool result')
    await expect(fieldSection).toContainText('Lookup')
    await expect(fieldSection).toContainText('tool-1')
    const context = dialog.locator('.context-box')
    await expect(context).toContainText('fourth line')
    await expect(context).toContainText('sixth line')
    await expect(context).not.toContainText('first line')

    const entry = dialog.locator('[data-slot="credential-entry"]')
    await expect(entry).toContainText('fourth line')
    await expect(entry).toContainText('other_secret')
    await expect(entry).not.toContainText('test_credential')
    await entry.getByRole('button', { name: 'Show matched text' }).click()
    await expect(entry).toContainText('test_credential')
    await expect(entry.getByRole('button', { name: 'Hide matched text' })).toBeVisible()

    const hitMatch = dialog.getByRole('button', { name: /\(fixture-rule-0\)/ })
    await expect(hitMatch).toHaveAttribute('data-emphasis', 'hit')
    const otherMatch = dialog.getByRole('button', { name: /\(fixture-rule-1\)/ })
    await expect(otherMatch).toHaveAttribute('data-emphasis', 'match')

    await page.keyboard.press('Escape')
    await expect(dialog).toHaveCount(0)
    await expect(row).toBeFocused()

    const reopened = await openRow(page)
    await expect(reopened.locator('[data-slot="credential-entry"]')).not.toContainText('test_credential')
    await expect(reopened.getByRole('button', { name: 'Show matched text' })).toBeVisible()
  })

  test('switches hits without leaking the previous entry and re-masks the snippet', async ({ page }) => {
    const state = await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          hits: [
            { rule_id: 'fixture-rule-0', source_type: 'user_message', latest_input: true },
            { rule_id: 'fixture-rule-1', source_type: 'tool_result' },
          ],
        }),
      ],
      detail: (_id, index) =>
        index === 0
          ? availableDetail({
              field: { source_type: 'user_message' },
              text: 'first entry\nsecret_one',
              matches: [
                {
                  rule_id: 'fixture-rule-0',
                  start: 12,
                  end: 22,
                  start_line: 2,
                  start_column: 1,
                  end_line: 2,
                  end_column: 11,
                  hit: true,
                },
              ],
            })
          : availableDetail({
              field: { source_type: 'tool_result', tool_id: 'tool-2', tool_name: 'Reader' },
              text: 'second entry\nsecret_two\nmore',
              matches: [
                {
                  rule_id: 'fixture-rule-1',
                  start: 13,
                  end: 23,
                  start_line: 2,
                  start_column: 1,
                  end_line: 2,
                  end_column: 11,
                  hit: true,
                },
              ],
            }),
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const dialog = await openRow(page)
    const entry = dialog.locator('[data-slot="credential-entry"]')
    await expect(entry).toContainText('first entry')
    await entry.getByRole('button', { name: 'Show matched text' }).click()
    await expect(entry).toContainText('secret_one')

    await chip(dialog, 'Fixture credential 1', 'Tool result').click()
    await expect(entry).toContainText('second entry')
    await expect(entry).not.toContainText('secret_one')
    await expect(entry).not.toContainText('secret_two')
    await expect(entry.getByRole('button', { name: 'Show matched text' })).toBeVisible()
    await expect(dialog.getByRole('heading', { name: 'Field context' })).toBeVisible()
    await expect(
      dialog.locator('section').filter({ has: page.getByRole('heading', { name: 'Field context' }) }),
    ).toContainText('Reader')
    expect(state.detailRequests).toEqual([
      { id: '100.0', index: 0 },
      { id: '100.0', index: 1 },
    ])
  })

  test('explains when the entry text is unavailable and why', async ({ page }) => {
    await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          hits: [
            { rule_id: 'fixture-rule-0', source_type: 'system_or_history' },
            { rule_id: 'fixture-rule-1', source_type: 'other_text' },
          ],
        }),
      ],
      detail: (_id, index) =>
        index === 0
          ? { state: 'unavailable', field: { source_type: 'system_or_history' }, reason: 'not_retained' }
          : { state: 'unavailable', field: { source_type: 'other_text' }, reason: 'missing' },
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const dialog = await openRow(page)
    await expect(dialog.getByText('Entry text unavailable', { exact: true })).toBeVisible()
    await expect(dialog.getByText('The text at this location is not part of the retained history.')).toBeVisible()
    await chip(dialog, 'Fixture credential 1', 'Other readable text').click()
    await expect(dialog.getByText('This observation has expired or was removed.')).toBeVisible()
    // 正文不可用时没有完整条目或匹配列表，也不报错。
    await expect(dialog.locator('[data-slot="credential-entry"]')).toHaveCount(0)
    await expect(dialog.getByRole('alert')).toHaveCount(0)
  })

  test('warns when the hit is located by rule, the entry is truncated, or no hit match remains', async ({ page }) => {
    await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          hits: [
            { rule_id: 'fixture-rule-0', source_type: 'tool_result' },
            { rule_id: 'fixture-rule-1', source_type: 'tool_result' },
          ],
        }),
      ],
      detail: (_id, index) =>
        index === 0
          ? availableDetail({ truncated: true, hit_basis: 'rule', matches: [availableDetail().matches[1]] })
          : availableDetail({ matches: [] }),
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const dialog = await openRow(page)
    await expect(dialog.getByRole('alert')).toContainText('located by rule')
    await expect(dialog.getByText('Only the beginning of this entry is available.')).toBeVisible()
    await expect(
      dialog.getByText('The detected credential is not in the available text.', { exact: false }),
    ).toBeVisible()
    await expect(dialog.getByRole('button', { name: /\(fixture-rule-1\)/ })).toHaveAttribute('data-emphasis', 'match')

    await chip(dialog, 'Fixture credential 1', 'Tool result').click()
    await expect(dialog.getByText('No rule matched this entry.')).toBeVisible()
  })

  test('lists deduplicated later reuse with links to request records', async ({ page }) => {
    const state = await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          rule_ids: ['fixture-rule-0'],
          hits: [{ rule_id: 'fixture-rule-0', source_type: 'tool_result' }],
        }),
      ],
      reuses: () => ({
        linkable: true,
        items: [
          reuse({ interaction_id: 'reuse-1', last_reused_at: createdAt - 10 }),
          reuse({ interaction_id: 'reuse-1', last_reused_at: createdAt - 100 }),
          reuse({
            interaction_id: 'reuse-2',
            source_type: 'user_message',
            tool_id: null,
            latest_input: true,
            last_reused_at: createdAt,
          }),
        ],
        truncated: true,
      }),
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const dialog = await openRow(page)
    const links = dialog.getByRole('link', { name: /Open request record/ })
    await expect(links).toHaveCount(2)
    await expect(links.first()).toHaveAttribute('href', '/logs?interaction=reuse-2&highlight=credential-hits')
    await expect(links.first()).toContainText('User message')
    await expect(links.last()).toHaveAttribute('href', '/logs?interaction=reuse-1&highlight=credential-hits')
    await expect(links.last()).toContainText('Tool arguments')
    await expect(links.last()).toContainText('Last reused')
    await expect(dialog.getByText('Only some reuse locations are shown.')).toBeVisible()
    // 不显示复用次数，只列出去重后的位置。
    expect(state.reuseRequests).toEqual(['100.0'])
  })

  test('explains an unlinkable mapping and an empty reuse history', async ({ page }) => {
    const state = await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          api_key_name: 'unlinkable-key',
          rule_ids: ['fixture-rule-0'],
          hits: [{ rule_id: 'fixture-rule-0', source_type: 'tool_result' }],
          linkable: false,
        }),
        discovery(1, {
          api_key_name: 'empty-key',
          rule_ids: ['fixture-rule-0'],
          hits: [{ rule_id: 'fixture-rule-0', source_type: 'tool_result' }],
        }),
      ],
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const unlinkable = await openRow(page, 'unlinkable-key')
    await expect(
      unlinkable.getByText('This mapping was created before the upgrade, so later reuse cannot be linked.'),
    ).toBeVisible()
    expect(state.reuseRequests).toEqual([])
    await page.keyboard.press('Escape')
    await expect(page.getByRole('dialog')).toHaveCount(0)

    const empty = await openRow(page, 'empty-key')
    await expect(empty.getByText('No later reuse.')).toBeVisible()
    expect(state.reuseRequests).toEqual(['100.1'])
  })

  test('tells an expired hit from a temporary detail failure and retries the latter', async ({ page }) => {
    let detailAttempts = 0
    await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          hits: [
            { rule_id: 'fixture-rule-0', source_type: 'tool_result' },
            { rule_id: 'fixture-rule-1', source_type: 'tool_result' },
          ],
        }),
      ],
      detail: (_id, index) => {
        if (index === 0) return { status: 404 }
        detailAttempts += 1
        return detailAttempts === 1 ? { status: 503 } : availableDetail()
      },
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const dialog = await openRow(page)
    await expect(dialog.getByText('This hit is no longer available.')).toBeVisible()
    await expect(dialog.getByRole('alert')).toHaveCount(0)

    await chip(dialog, 'Fixture credential 1', 'Tool result').click()
    await expect(dialog.getByRole('alert')).toContainText('Hit details could not be loaded.')
    await dialog.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect(dialog.locator('[data-slot="credential-entry"]')).toContainText('fourth line')
    await expect(dialog.getByRole('alert')).toHaveCount(0)
  })

  test('opens the matched rule from its chip without opening the hit panel', async ({ page }) => {
    await mockRedaction(page, {
      discoveries: [
        discovery(0, {
          rule_ids: ['fixture-rule-0'],
          hits: [{ rule_id: 'fixture-rule-1', source_type: 'tool_result' }],
        }),
      ],
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await chip(page.getByRole('row'), 'Fixture credential 1', 'Tool result').click()
    const sheet = page.getByRole('dialog')
    await expect(sheet.getByRole('heading', { name: 'Fixture credential 1' })).toBeVisible()
    await expect(sheet.getByText('fixture-rule-1', { exact: true })).toBeVisible()
    await sheet.getByRole('button', { name: 'Close' }).click()
    await expect(sheet).toHaveCount(0)
    await expect(page.getByRole('tab', { name: 'Hit records' })).toHaveAttribute('aria-selected', 'true')
  })

  test('loads further pages on scroll and renders only the rows near the viewport', async ({ page }) => {
    const state = await mockRedaction(page, { discoveryPage: pagedDiscoveries(70) })
    await page.setViewportSize({ width: 1280, height: 700 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'key-0')).toBeVisible()
    expect(state.discoveryRequests.map((request) => request.cursor)).toEqual([null])
    expect(await page.getByRole('row').count()).toBeLessThan(25)

    await expect(async () => {
      await scrollHits(page, 'bottom')
      await expect(rowOf(page, 'key-69')).toBeVisible({ timeout: 1000 })
    }).toPass()
    expect(state.discoveryRequests.map((request) => request.cursor)).toEqual([null, '30', '60'])
    // 70 条记录全部加载后，DOM 里仍只有视口附近的行。
    expect(await page.getByRole('row').count()).toBeLessThan(25)
    await expect(rowOf(page, 'key-0')).toHaveCount(0)
    expect(await page.evaluate(() => document.documentElement.scrollHeight <= innerHeight + 1)).toBe(true)
  })

  test('sizes rows to their content and never lets neighbouring rows overlap', async ({ page }) => {
    await mockRedaction(page, {
      discoveries: [
        discovery(0, { api_key_name: 'key-0', hits: [{ rule_id: 'fixture-rule-0', source_type: 'tool_result' }] }),
        discovery(1, {
          api_key_name: 'key-1',
          hits: Array.from({ length: 9 }, (_, index) => ({
            rule_id: `fixture-rule-${index}`,
            source_type: ['tool_result', 'tool_arguments', 'system_or_history'][index % 3],
          })),
        }),
        discovery(2, { api_key_name: 'key-2', hits: [{ rule_id: 'fixture-rule-1', source_type: 'tool_result' }] }),
      ],
    })
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/reversible-redaction')
    await expect(async () => {
      const [first, tall, last] = await Promise.all(['key-0', 'key-1', 'key-2'].map((name) => box(rowOf(page, name))))
      expect(tall.height).toBeGreaterThan(first.height * 1.5)
      expect(overlaps(first, tall)).toBe(false)
      expect(overlaps(tall, last)).toBe(false)
    }).toPass()
  })

  test('shows an error with Retry when the first page cannot be loaded', async ({ page }) => {
    await mockRedaction(page, {
      discoveryPage: (_query, call) => (call === 1 ? { status: 503 } : { items: [discovery(0)] }),
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(page.getByRole('alert')).toContainText('Hit records are unavailable.')
    await expect(page.getByText('No credential mappings were created in this period.', { exact: false })).toHaveCount(0)
    await page.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect(rowOf(page, 'omp-key')).toBeVisible()
    await expect(page.getByRole('alert')).toHaveCount(0)
  })

  test('keeps loaded rows and offers Retry when a further page fails', async ({ page }) => {
    const state = await mockRedaction(page, {
      discoveryPage: (query, call) => (call === 2 ? { status: 503 } : pagedDiscoveries(40)(query)),
    })
    await page.setViewportSize({ width: 1280, height: 700 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'key-0')).toBeVisible()
    await scrollHits(page, 'bottom')
    const alert = page.getByRole('alert')
    await expect(alert).toContainText('Hit records are unavailable.')
    await expect(rowOf(page, 'key-29')).toBeVisible()
    await page.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect(async () => {
      await scrollHits(page, 'bottom')
      await expect(rowOf(page, 'key-39')).toBeVisible({ timeout: 1000 })
    }).toPass()
    await expect(alert).toHaveCount(0)
    expect(state.discoveryRequests.map((request) => request.cursor)).toEqual([null, '30', '30'])
  })

  test('refreshes every 30 seconds at the top, merges new rows in front and ignores a failed refresh', async ({
    page,
  }) => {
    await page.clock.install({ time: new Date('2026-10-27T09:00:00Z') })
    const state = await mockRedaction(page, {
      discoveryPage: (_query, call) => {
        if (call === 2) return { status: 503 }
        return { items: call === 1 ? [discovery(1)] : [discovery(0, { api_key_name: 'fresh-key' }), discovery(1)] }
      },
    })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'omp-key')).toBeVisible()
    await page.clock.pauseAt(new Date('2026-10-27T09:00:10Z'))

    await page.clock.runFor(30_000)
    await expect.poll(() => state.discoveryRequests.length).toBe(2)
    // 后台刷新失败时保留现有列表，不弹出错误。
    await expect(page.getByRole('alert')).toHaveCount(0)
    await expect(rowOf(page, 'omp-key')).toBeVisible()

    await page.clock.runFor(30_000)
    await expect.poll(() => state.discoveryRequests.length).toBe(3)
    await expect(rowOf(page, 'fresh-key')).toBeVisible()
    await expect(rowOf(page, 'omp-key')).toHaveCount(1)
    expect((await box(rowOf(page, 'fresh-key'))).y).toBeLessThan((await box(rowOf(page, 'omp-key'))).y)
  })

  test('stops refreshing away from the top, on another tab or in a hidden page, then catches up', async ({ page }) => {
    await page.clock.install({ time: new Date('2026-10-27T09:00:00Z') })
    const state = await mockRedaction(page, {
      discoveries: Array.from({ length: 40 }, (_, index) => discovery(index, { api_key_name: `key-${index}` })),
    })
    await page.setViewportSize({ width: 1280, height: 700 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'key-0')).toBeVisible()
    await page.clock.pauseAt(new Date('2026-10-27T09:00:10Z'))
    const requests = () => state.discoveryRequests.length
    // 回到可刷新状态后，距上次刷新已超过一个周期，应立刻补一次刷新。
    const catchUp = async (expected: number) => {
      await expect(async () => {
        await page.clock.runFor(1)
        expect(requests()).toBe(expected)
      }).toPass()
    }

    await scrollHits(page, 2000)
    await expect(rowOf(page, 'key-0')).toHaveCount(0)
    await page.clock.runFor(120_000)
    expect(requests()).toBe(1)
    await scrollHits(page, 'top')
    await catchUp(2)

    await page.getByRole('tab', { name: /Current rules/ }).click()
    await page.clock.runFor(120_000)
    expect(requests()).toBe(2)
    await page.getByRole('tab', { name: 'Hit records' }).click()
    await catchUp(3)

    const setVisibility = (mode: 'hidden' | 'visible') =>
      page.evaluate((next) => {
        Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => next })
        document.dispatchEvent(new Event('visibilitychange'))
      }, mode)
    await setVisibility('hidden')
    await page.clock.runFor(120_000)
    expect(requests()).toBe(3)
    await setVisibility('visible')
    await catchUp(4)
  })

  for (const width of [1440, 1280]) {
    test(`fits every column without horizontal scrolling at ${width}px`, async ({ page }) => {
      await mockRedaction(page, {
        discoveries: [
          discovery(0, {
            hits: [
              { rule_id: 'fixture-rule-0', source_type: 'user_message', latest_input: true },
              { rule_id: 'fixture-rule-1', source_type: 'tool_arguments' },
            ],
            rule_sets: ['custom', 'builtin'],
          }),
          discovery(1, {
            api_key_name: 'Second fixture key',
            rule_ids: ['fixture-rule-0', 'fixture-rule-1'],
            hits: Array.from({ length: 4 }, (_, index) => ({
              rule_id: `fixture-rule-${index}`,
              source_type: 'system_or_history',
            })),
          }),
        ],
      })
      await page.setViewportSize({ width, height: 800 })
      await page.goto('/reversible-redaction')
      const panel = page.getByRole('tabpanel')
      await expect(panel.getByRole('table')).toBeVisible()
      expect(await hitViewport(page).evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true)
      // 行内不再提供跳转箭头；请求入口只出现在侧栏的复用列表。
      await expect(panel.getByRole('link')).toHaveCount(0)
      await expect(rowOf(page, 'omp-key').getByRole('cell').nth(2)).toContainText('Built-in')
      await expect(rowOf(page, 'omp-key').getByRole('cell').nth(2)).toContainText('Custom')
    })
  }

  test('keeps a short history at its natural height', async ({ page }) => {
    await mockRedaction(page, { discoveries: [discovery(0), discovery(1)] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    const table = page.getByRole('table', { name: 'Hit records' })
    await expect(rowOf(page, 'omp-key').first()).toBeVisible()
    expect((await box(table)).height).toBeLessThan(300)
  })

  for (const width of [390, 320]) {
    test(`reflows into stacked rows without horizontal overflow at ${width}px`, async ({ page }) => {
      await mockRedaction(page, {
        discoveries: [
          discovery(0, {
            rule_ids: ['fixture-rule-0'],
            hits: [{ rule_id: 'fixture-rule-0', source_type: 'user_message', latest_input: true }],
            omitted_hits: 2,
          }),
          discovery(1, { api_key_name: 'second-key', rule_ids: ['fixture-rule-1'] }),
        ],
      })
      await page.setViewportSize({ width, height: 800 })
      await page.goto('/reversible-redaction')
      const row = rowOf(page, 'omp-key')
      await expect(page.getByRole('columnheader')).toHaveCount(0)
      await expect(row.getByText('Rule sets', { exact: true })).toBeVisible()
      await expect(row.getByText('Created by', { exact: true })).toBeVisible()
      await expect(row.getByText('Hits', { exact: true })).toBeVisible()
      await expect(chip(row, 'Fixture credential 0', 'User message')).toBeVisible()
      await expect(row.getByText('+2', { exact: true })).toBeVisible()
      await expect(row.getByRole('link')).toHaveCount(0)
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
    })
  }
})

test.describe('hit records time range', () => {
  const HOUR = 3_600_000
  const frozen = new Date('2026-10-27T09:00:00Z')
  const frozenMs = frozen.getTime()
  const near = (actual: number | null, expected: number) =>
    actual !== null && Math.abs(actual - expected) < 60_000

  test('lists the last 7 days by default and keeps the time controls on this tab only', async ({ page }) => {
    await page.clock.install({ time: frozen })
    const state = await mockRedaction(page, { discoveries: [discovery(0)] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'omp-key')).toBeVisible()
    expect(state.discoveryRequests).toHaveLength(1)
    // 实时窗口只带起点；终点是“现在”，新命中不会被终点挡住。
    expect(near(state.discoveryRequests[0].startAt, frozenMs - 168 * HOUR)).toBe(true)
    expect(state.discoveryRequests[0].endAt).toBeNull()
    await expect(page.getByRole('button', { name: 'Hit records time range' })).toContainText('Last 7d')
    await expect(page.getByRole('button', { name: 'Reload hit records' })).toBeVisible()
    await expect(page.locator('#credential-rule-search')).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Add custom rule' })).toHaveCount(0)

    await openRules(page)
    await expect(page.getByRole('button', { name: 'Hit records time range' })).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Reload hit records' })).toHaveCount(0)
    await expect(page.locator('#credential-rule-search')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Add custom rule' })).toBeVisible()

    await page.getByRole('tab', { name: 'Matching test', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Hit records time range' })).toHaveCount(0)
    await expect(page.locator('#credential-rule-search')).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Add custom rule' })).toHaveCount(0)
  })

  test('reloads from the first page when a preset is chosen and steps through older and newer windows', async ({
    page,
  }) => {
    await page.clock.install({ time: frozen })
    const state = await mockRedaction(page, { discoveries: [discovery(0)] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'omp-key')).toBeVisible()

    await page.getByRole('button', { name: 'Hit records time range' }).click()
    await page.getByRole('option', { name: 'Last 6h' }).click()
    await expect.poll(() => state.discoveryRequests.length).toBe(2)
    expect(near(state.discoveryRequests[1].startAt, frozenMs - 6 * HOUR)).toBe(true)
    expect(state.discoveryRequests[1].endAt).toBeNull()
    await expect(page.getByRole('button', { name: 'Newer', exact: true })).toBeDisabled()

    // 较早：紧邻当前实时窗口之前的 6 小时，起止都固定。
    await page.getByRole('button', { name: 'Older', exact: true }).click()
    await expect.poll(() => state.discoveryRequests.length).toBe(3)
    const older = state.discoveryRequests[2]
    expect(near(older.endAt, frozenMs - 6 * HOUR)).toBe(true)
    expect(older.startAt).toBe((older.endAt ?? 0) - 6 * HOUR)
    await expect(page.getByRole('button', { name: 'Newer', exact: true })).toBeEnabled()

    // 较新追上现在时回到实时窗口，只带起点。
    await page.getByRole('button', { name: 'Newer', exact: true }).click()
    await expect.poll(() => state.discoveryRequests.length).toBe(4)
    expect(near(state.discoveryRequests[3].startAt, frozenMs - 6 * HOUR)).toBe(true)
    expect(state.discoveryRequests[3].endAt).toBeNull()
    await expect(page.getByRole('button', { name: 'Newer', exact: true })).toBeDisabled()
  })

  test('applies a custom range exactly as entered and rejects an end that is not after the start', async ({ page }) => {
    await page.clock.install({ time: frozen })
    const state = await mockRedaction(page, { discoveries: [discovery(0)] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'omp-key')).toBeVisible()

    await page.getByRole('button', { name: 'Choose date and time range' }).click()
    const dialog = page.getByRole('dialog')
    await dialog.getByLabel('Start time').fill('2026-10-01T08:00')
    await dialog.getByLabel('End time').fill('2026-10-01T08:00')
    await expect(dialog.getByRole('alert')).toContainText('End time must be after start time')
    await expect(dialog.getByRole('button', { name: 'Apply' })).toBeDisabled()

    // 跨度不设上限：这里是 10 天。
    await dialog.getByLabel('End time').fill('2026-10-11T08:00')
    await expect(dialog.getByRole('alert')).toHaveCount(0)
    await dialog.getByRole('button', { name: 'Apply' }).click()
    await expect(dialog).toHaveCount(0)
    await expect.poll(() => state.discoveryRequests.length).toBe(2)
    const [start, end] = [new Date('2026-10-01T08:00'), new Date('2026-10-11T08:00')]
    expect(state.discoveryRequests[1]).toMatchObject({ cursor: null, startAt: start.getTime(), endAt: end.getTime() })
    await expect(page.getByRole('button', { name: 'Hit records time range' })).toContainText('Custom')
  })

  test('explains an empty range and reloads on request', async ({ page }) => {
    await page.clock.install({ time: frozen })
    const state = await mockRedaction(page, { discoveryPage: (_query, call) => ({ items: call === 1 ? [] : [discovery(0)] }) })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(page.getByText('No hit records in this time range')).toBeVisible()
    await expect(page.getByText('No credential mappings were created in this period.', { exact: false })).toBeVisible()
    await page.getByRole('button', { name: 'Reload hit records' }).click()
    await expect(rowOf(page, 'omp-key')).toBeVisible()
    expect(state.discoveryRequests).toHaveLength(2)
    await expect(page.getByText('No hit records in this time range')).toHaveCount(0)
  })

  test('does not refresh a fixed window in the background', async ({ page }) => {
    await page.clock.install({ time: frozen })
    const state = await mockRedaction(page, { discoveries: [discovery(0)] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await expect(rowOf(page, 'omp-key')).toBeVisible()
    await page.getByRole('button', { name: 'Older', exact: true }).click()
    await expect.poll(() => state.discoveryRequests.length).toBe(2)
    await page.clock.pauseAt(new Date('2026-10-27T09:00:10Z'))
    await page.clock.runFor(120_000)
    expect(state.discoveryRequests).toHaveLength(2)
  })
})

test.describe('rules workspace', () => {
  test('fills the 120rem container and the remaining height at 2560px with content-sized columns', async ({ page }) => {
    await mockRedaction(page)
    await page.setViewportSize({ width: 2560, height: 720 })
    await page.goto('/reversible-redaction')
    await openRules(page)
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
    // 分页器贴在工作区底部：只隔着外框边线、分页器内边距与主内容区的正常底部内边距，没有大块留白。
    const main = await box(page.getByRole('main'))
    const paginator = await box(page.locator('[data-slot="data-table-paginator"]'))
    const gap = main.y + main.height - paginator.y - paginator.height
    expect(gap).toBeGreaterThanOrEqual(0)
    expect(gap).toBeLessThanOrEqual(36)
  })

  for (const width of [390, 320]) {
    test(`keeps tabs, search, filter, add and paginator apart and operable at ${width}px`, async ({ page }) => {
      await mockRedaction(page)
      await page.setViewportSize({ width, height: 800 })
      await page.goto('/reversible-redaction')
      await openRules(page)
      const tabs = await box(page.getByRole('tablist'))
      const search = await box(page.locator('#credential-rule-search').locator('xpath=ancestor::*[@data-slot="input-group"]'))
      const filter = await box(page.getByLabel('Rule source'))
      const add = await box(page.getByRole('button', { name: 'Add custom rule' }))
      expect(search.y).toBeGreaterThanOrEqual(tabs.y + tabs.height - 0.5)
      expect(filter.y).toBeGreaterThanOrEqual(search.y + search.height - 0.5)
      expect(add.y).toBeGreaterThanOrEqual(filter.y + filter.height - 0.5)
      expect(overlaps(search, filter)).toBe(false)
      expect(overlaps(filter, add)).toBe(false)
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)

      const next = page.getByRole('button', { name: 'Next', exact: true })
      await next.scrollIntoViewIfNeeded()
      await expect(next).toBeInViewport()
      await next.click()
      await expect(page.getByText('Page 2 of 2')).toBeVisible()
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true)
    })
  }

  test('keeps a valid page after filtering and searching from the last page', async ({ page }) => {
    await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1440, height: 900 })
    await page.goto('/reversible-redaction')
    await openRules(page)
    await page.getByRole('button', { name: 'Last page', exact: true }).click()
    await expect(page.getByText('Page 2 of 2')).toBeVisible()
    await page.locator('#credential-rule-search').fill('Fixture credential 3')
    // Fixture credential 3、30–34 共 6 条，回到有效的第 1 页并显示数据。
    await expect(page.getByText('Page 1 of 1')).toBeVisible()
    await expect(page.getByRole('button', { name: /Fixture credential 34/ })).toBeVisible()
    await page.locator('#credential-rule-search').fill('')
    await page.getByRole('button', { name: 'Last page', exact: true }).click()
    await expect(page.getByText(/Page 2 of 2/)).toBeVisible()
    await page.getByLabel('Rule source').click()
    await page.getByRole('option', { name: 'Custom rules' }).click()
    await expect(page.getByText('Page 1 of 1')).toBeVisible()
    await expect(page.getByRole('button', { name: /Internal token/ })).toBeVisible()
  })

  test('keeps the paginator reachable when the window is low', async ({ page }) => {
    await mockRedaction(page)
    await page.setViewportSize({ width: 1280, height: 420 })
    await page.goto('/reversible-redaction')
    await openRules(page)
    await expect(page.getByRole('table').getByRole('button', { name: /Fixture credential 0/ })).toBeVisible()
    const next = page.getByRole('button', { name: 'Next', exact: true })
    await next.scrollIntoViewIfNeeded()
    await expect(next).toBeInViewport()
    // 低窗口保留至少 24rem 工作区并允许页面滚动。
    const workspace = await box(page.locator('[data-slot="data-table"]'))
    expect(workspace.height).toBeGreaterThanOrEqual(24 * 16 - 2)
    await next.click()
    await expect(page.getByText('Page 2 of 2')).toBeVisible()
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
    await openRules(page)
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
    await openRules(page)
    await page.getByRole('button', { name: 'Add custom rule' }).click()
    const sheet = sheetOf(page)
    await expect(sheet.getByRole('heading', { name: 'Add custom rule' })).toBeVisible()
    // 没有常驻说明文字：标题之外既无 Sheet 描述也无字段说明。
    await expect(sheet.locator('[data-slot="sheet-description"]')).toHaveCount(0)
    await expect(sheet.locator('[data-slot="field-description"]')).toHaveCount(0)
    const helps = sheet.getByRole('button', { name: 'More about this field' })
    await helps.first().focus()
    await expect(page.getByText('Type the text to protect. It')).toBeVisible()
    await page.keyboard.press('Escape')
    await helps.nth(1).focus()
    await expect(page.getByText('Case-sensitive and matched as typed')).toBeVisible()
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
    await openRules(page)
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

  test('keeps the editor and typed secret when an edit fails', async ({ page }) => {
    const state = await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/reversible-redaction')
    await openRules(page)
    await page.getByRole('button', { name: 'Internal token' }).click()
    const sheet = sheetOf(page)
    await expect(sheet.getByRole('heading', { name: 'Edit custom rule' })).toBeVisible()
    await sheet.getByLabel('Rule name').fill('Renamed token')
    await sheet.getByRole('button', { name: 'Show secret' }).click()
    const secret = sheet.getByLabel('Text to protect')
    await secret.fill('renamed-secret-value')

    state.failNextWrite = { status: 503, json: { error: 'Fixture edit unavailable' } }
    await sheet.getByRole('button', { name: 'Save rule' }).click()
    await expect(sheet.getByRole('alert').filter({ hasText: 'Fixture edit unavailable' })).toBeVisible()
    await expect(sheet).toBeVisible()
    await expect(sheet.getByLabel('Rule name')).toHaveValue('Renamed token')
    await expect(secret).toHaveValue('renamed-secret-value')
    expect(state.writes.at(-1)).toMatchObject({
      method: 'PUT',
      path: '/api/v1/reversible-redaction/custom-rules/custom-1',
      body: { name: 'Renamed token', spec: { mode: 'simple', text: 'renamed-secret-value' } },
    })

    await sheet.getByRole('button', { name: 'Save rule' }).click()
    await expect(sheet).toBeHidden()
    await expect(page.getByRole('button', { name: /Renamed token/ })).toBeVisible()
  })

  test('keeps the editor when a delete fails and removes the named rule when it succeeds', async ({ page }) => {
    const state = await mockRedaction(page, { custom: [savedRule] })
    await page.setViewportSize({ width: 1280, height: 800 })
    await page.goto('/reversible-redaction')
    await openRules(page)
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
    await openRules(page)
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
