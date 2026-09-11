import { expect, test, type Locator, type Page } from '@playwright/test'

import type {
  ConfirmedUsage,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  ObservationEvent,
  RunDetail,
} from '../src/lib/types'
import { prepareApp } from './prepare-app'

const DAY = 86_400_000
const startedAt = Date.UTC(2026, 8, 6, 8, 0, 0)
const routeIds = {
  atlas: '11111111-1111-4111-8111-111111111111',
  boreal: '22222222-2222-4222-8222-222222222222',
  cinder: '33333333-3333-4333-8333-333333333333',
  delta: '44444444-4444-4444-8444-444444444444',
  ember: '55555555-5555-4555-8555-555555555555',
} as const
const statusLabels: Record<string, string> = {
  completed: 'Completed',
  interrupted: 'Interrupted',
  running: 'Running',
  waiting_client: 'Waiting for client',
}

const usage: ConfirmedUsage = {
  input_tokens: 1_240,
  output_tokens: 86,
  cache_read_tokens: 320,
  cache_write_tokens: null,
  reasoning_tokens: 44,
}

function interaction(
  id: string,
  rootId: string,
  parentId: string | null,
  model: string,
  routeId: string,
  status: string,
  offset: number,
): InteractionSummary {
  return {
    id,
    root_id: rootId,
    parent_interaction_id: parentId,
    generation_root_id: rootId,
    first_route_id: routeId,
    first_model_display_name: model,
    status,
    started_at: startedAt + offset,
    last_active_at: startedAt + offset + 10_000,
    input_preview:
      model === 'Boreal'
        ? null
        : `**${model} user question**\n\nPlease explain the result using \`Markdown\`.\n\n${Array.from({ length: 20 }, (_, index) => `Input context ${index}`).join('\n\n')}\n\nInput preview ending`,
    visible_tail: `${model} client-visible answer`,
    usage,
    debug_status: 'none',
    observation_gap: false,
    matched: true,
    last_event_sequence: 10,
    context_events: [],
  }
}

function runFor(item: InteractionSummary): RunDetail {
  const event: ObservationEvent = {
    sequence: item.last_event_sequence,
    occurred_at: item.last_active_at,
    interaction_id: item.id,
    run_id: `run-${item.id}`,
    rejection_id: null,
    kind: item.status === 'running' ? 'model_turn_started' : 'delivery_terminal',
    payload: item.status === 'running' ? { model_turn_id: `turn-${item.id}` } : { delivered: true },
  }
  return {
    id: `run-${item.id}`,
    parent_run_id: null,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: item.first_route_id,
    model_display_name: item.first_model_display_name,
    ingress_protocol: 'openai-responses',
    status: item.status,
    terminal_reason: item.status === 'running' || item.status === 'waiting_client' ? null : 'completed',
    user_interrupted: false,
    debug_enabled: false,
    client_output_committed: item.status === 'completed',
    started_at: item.started_at,
    finished_at: item.status === 'completed' ? item.last_active_at : null,
    usage: item.usage,
    events: [
      {
        ...event,
        sequence: item.last_event_sequence - 1,
        kind: 'client_visible_content_delta',
        payload: { text: item.visible_tail },
      },
      event,
    ],
    trace: null,
    debug_events: [],
  }
}

interface ObservationFixture {
  emit(event: ObservationEvent, visibleTail?: string): void
  releaseRemainingRoots(): void
  forestRequests: URL[]
  debugWrites: Array<{ enabled: boolean; confirmed: boolean }>
}

async function installObservationFixture(
  page: Page,
  holdRemainingRoots = false,
  captureDebug = false,
  emptyRunningOutput = false,
  transformDetail?: (detail: InteractionDetail) => void,
): Promise<ObservationFixture> {
  await page.clock.setFixedTime(startedAt + 300_000)
  let releaseRemainingRoots!: () => void
  const remainingRootsReady = new Promise<void>((resolve) => {
    releaseRemainingRoots = resolve
  })
  if (!holdRemainingRoots) releaseRemainingRoots()
  const atlas = interaction('interaction-atlas', 'root-a', null, 'Atlas', routeIds.atlas, 'completed', 0)
  const boreal = interaction(
    'interaction-boreal',
    'root-a',
    atlas.id,
    'Boreal',
    routeIds.boreal,
    'waiting_client',
    60_000,
  )
  const cinder = interaction('interaction-cinder', 'root-a', atlas.id, 'Cinder', routeIds.cinder, 'running', 120_000)
  if (emptyRunningOutput) cinder.visible_tail = ''
  const contextSiblings = ['Context One', 'Context Two', 'Context Three', 'Context Four', 'Context Five'].map(
    (model, index) =>
      interaction(
        `interaction-context-${index + 1}`,
        'root-a',
        atlas.id,
        model,
        routeIds.atlas,
        'completed',
        10_000 + index * 5_000,
      ),
  )
  const delta = interaction('interaction-delta', 'root-b', null, 'Delta', routeIds.delta, 'completed', 180_000)
  const ember = interaction('interaction-ember', 'root-c', null, 'Ember', routeIds.ember, 'completed', 240_000)
  const roots: ForestRoot[] = [
    { id: 'root-a', last_active_at: cinder.last_active_at, interactions: [atlas, boreal, cinder, ...contextSiblings] },
    { id: 'root-b', last_active_at: delta.last_active_at, interactions: [delta] },
    { id: 'root-c', last_active_at: ember.last_active_at, interactions: [ember] },
  ]
  const recordedRuns = new Map(roots.flatMap((root) => root.interactions).map((item) => [item.id, runFor(item)]))
  const forestRequests: URL[] = []
  const debugWrites: Array<{ enabled: boolean; confirmed: boolean }> = []
  const streamEvents: ObservationEvent[] = []
  let debugEnabled = false
  let snapshotSequence = 10

  const rootFor = (id: string, status?: string | null): ForestRoot => {
    const root = roots.find((candidate) => candidate.interactions.some((item) => item.id === id))!
    return {
      ...root,
      interactions: root.interactions.map((item) => ({ ...item, matched: !status || item.status === status })),
    }
  }

  await page.route('**/api/v1/observations/**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname.replace('/api/v1', '')

    if (path === '/observations/events') {
      const event = streamEvents.shift()
      const body = event ? `id: ${event.sequence}\nevent: observation\ndata: ${JSON.stringify(event)}\n\n` : ''
      await route.fulfill({ contentType: 'text/event-stream', body })
      return
    }

    if (path === '/observations/debug') {
      if (request.method() === 'PUT') {
        const body = request.postDataJSON() as { enabled: boolean; confirmed: boolean }
        debugWrites.push(body)
        debugEnabled = body.enabled
      }
      await route.fulfill({
        json: {
          data: {
            enabled: debugEnabled,
            run_limit_bytes: 67_108_864,
            total_limit_bytes: 2_147_483_648,
            retained_bytes: 0,
            partial_trace_count: 0,
            retention_days: 7,
          },
        },
      })
      return
    }

    const detailMatch = path.match(/^\/observations\/interactions\/([^/]+)$/)
    if (detailMatch) {
      const id = decodeURIComponent(detailMatch[1])
      const status = url.searchParams.get('status')
      const root = rootFor(id, status)
      const selected = root.interactions.find((item) => item.id === id)!
      const detail: InteractionDetail = {
        interaction: selected,
        root,
        runs: [
          {
            ...recordedRuns.get(selected.id)!,
            debug_enabled: captureDebug,
            debug_events: captureDebug ? [{ fixture: 'retained diagnostic record' }] : [],
          },
        ],
        snapshot_sequence: snapshotSequence,
      }
      transformDetail?.(detail)
      await route.fulfill({ json: { data: detail } })
      return
    }

    if (path === '/observations/interactions') {
      forestRequests.push(url)
      const start = Number(url.searchParams.get('start_at'))
      const end = Number(url.searchParams.get('end_at'))
      const status = url.searchParams.get('status')
      const cursor = url.searchParams.get('cursor')
      if (cursor) await remainingRootsReady
      const matchingRoots = roots.filter((root) => {
        const latest = Math.max(...root.interactions.map((item) => item.last_active_at))
        return latest >= start && latest < end && (!status || root.interactions.some((item) => item.status === status))
      })
      const pageRoots = (cursor === 'remaining-roots' ? matchingRoots.slice(1) : matchingRoots.slice(0, 1)).map(
        (root) => rootFor(root.interactions[0].id, status),
      )
      await route.fulfill({
        json: {
          data: {
            anchor_at: end,
            window_index: 0,
            window_start: start,
            window_end: end,
            roots: pageRoots,
            root_total: matchingRoots.length,
            next_cursor: matchingRoots.length > 1 && !cursor ? 'remaining-roots' : null,
            snapshot_sequence: snapshotSequence,
          },
        },
      })
      return
    }

    await route.fallback()
  })

  return {
    releaseRemainingRoots,
    emit(event, visibleTail) {
      snapshotSequence = Math.max(snapshotSequence, event.sequence)
      const known = roots.flatMap((root) => root.interactions).find((item) => item.id === event.interaction_id)
      if (known) {
        known.last_event_sequence = event.sequence
        known.last_active_at = event.occurred_at
        const run = recordedRuns.get(known.id)!
        run.events.push(event)
        if (event.kind === 'client_visible_content_delta') {
          const payload = event.payload as { text: string }
          known.visible_tail = (known.visible_tail + payload.text).slice(-4096)
        } else {
          known.visible_tail = visibleTail ?? `Updated at sequence ${event.sequence}`
        }
        if (event.kind === 'delivery_terminal') {
          known.status = 'completed'
          run.status = 'completed'
          run.finished_at = event.occurred_at
        }
      }
      streamEvents.push(event)
    },
    forestRequests,
    debugWrites,
  }
}

function node(page: Page, name: string, status: string): Locator {
  return page.getByRole('button', { name: `${name}, ${statusLabels[status] ?? status}`, exact: true })
}

async function viewportTransform(page: Page): Promise<{ x: number; y: number; zoom: number }> {
  return page.locator('.svelte-flow__viewport').evaluate((element) => {
    const matrix = new DOMMatrixReadOnly(getComputedStyle(element).transform)
    return { x: matrix.e, y: matrix.f, zoom: matrix.a }
  })
}

test.describe('Interaction Observation canvas', () => {
  test.beforeEach(async ({ page }) => {
    await prepareApp(page)
  })

  test('fills remaining window space in both tabs and scrolls rejected requests only inside the list', async ({
    page,
  }) => {
    await installObservationFixture(page)
    await page.route('**/api/v1/observations/rejections?*', (route) =>
      route.fulfill({
        json: {
          data: {
            items: Array.from({ length: 30 }, (_, index) => ({
              id: `rejection-${index}`,
              occurred_at: startedAt,
              method: 'POST',
              path: `/v1/responses/${index}`,
              stage: 'decode',
              code: 'invalid_request',
              status_code: 400,
              debug_status: 'disabled',
            })),
            total: 31,
            next_cursor: 'more',
            snapshot_sequence: 0,
          },
        },
      }),
    )
    await page.goto('/logs')
    await expect(node(page, 'Atlas', 'completed')).toBeVisible()

    for (const viewport of [
      { width: 1440, height: 1200 },
      { width: 1280, height: 640 },
      { width: 500, height: 800 },
      { width: 320, height: 740 },
    ]) {
      await page.setViewportSize(viewport)
      for (const tab of ['Interaction Chains', 'Rejected Requests']) {
        await page.getByRole('tab', { name: tab, exact: true }).click()
        await expect
          .poll(() =>
            page.locator('.observation-workspace').evaluate((workspace) => {
              const main = document.querySelector('main')!
              const bottom = main.getBoundingClientRect().bottom - parseFloat(getComputedStyle(main).paddingBottom)
              return {
                gap: Math.round(bottom - workspace.getBoundingClientRect().bottom),
                overflow: main.scrollHeight - main.clientHeight,
                horizontalOverflow: main.scrollWidth - main.clientWidth,
              }
            }),
          )
          .toEqual({ gap: 0, overflow: 0, horizontalOverflow: 0 })
      }
      const list = page.locator('.rejection-list')
      await expect(list).toBeVisible()
      await page.locator('.rejections-view').evaluate((element) => {
        element.scrollTop = element.scrollHeight
      })
      await expect(list.getByRole('button').last()).toBeInViewport()
      await expect(page.getByRole('button', { name: 'Load more', exact: true })).toBeInViewport()
    }
  })

  test('opens readable conversation bubbles while retaining raw events in diagnostics', async ({ page }) => {
    await installObservationFixture(page, false, true)
    await page.goto('/logs')
    await node(page, 'Atlas', 'completed').getByRole('heading', { name: 'Atlas', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    const conversation = inspector.getByRole('log', { name: 'Conversation' })
    await expect(conversation.getByRole('article', { name: 'You', exact: true })).toContainText('Atlas user question')
    await expect(conversation.getByRole('article', { name: 'Atlas', exact: true })).toContainText(
      'Atlas client-visible answer',
    )
    await expect(inspector.getByText('run-interaction-atlas', { exact: true })).toBeHidden()
    await expect(inspector.getByText('retained diagnostic record', { exact: false })).toBeHidden()
    await inspector.getByRole('tab', { name: 'Diagnostics', exact: true }).click()
    await expect(inspector.getByText('run-interaction-atlas', { exact: true })).toBeHidden()
    for (const disclosure of await inspector
      .getByRole('button', { name: 'Technical identifiers', exact: true })
      .all()) {
      await disclosure.click()
    }
    await expect(inspector.getByText('run-interaction-atlas', { exact: true })).toBeVisible()
    await inspector.getByRole('tab', { name: 'Conversation', exact: true }).click()
    await expect(conversation.getByRole('article', { name: 'Atlas', exact: true })).toContainText(
      'Atlas client-visible answer',
    )
    await expect(inspector.getByText('run-interaction-atlas', { exact: true })).toBeHidden()
  })

  test('keeps diagnostic events readable while disclosing complete raw data and identifiers on demand', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page, false, true)
    const events: ObservationEvent[] = [
      {
        sequence: 11,
        occurred_at: startedAt + 299_000,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'run_admitted',
        payload: {
          route_id: routeIds.cinder,
          model_display_name: 'Cinder',
          debug_enabled: true,
          inferred_retry: false,
          parent_run_id: null,
          generation_parent_id: null,
          has_new_user: true,
          parent_interaction_id: 'interaction-atlas',
          root_id: 'root-a',
        },
      },
      {
        sequence: 12,
        occurred_at: startedAt + 299_100,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'generation_associated',
        payload: { root_id: 'generation-root-cinder', parent_id: 'generation-parent-atlas', has_new_user: true },
      },
      {
        sequence: 13,
        occurred_at: startedAt + 299_200,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'retained_tail_associated',
        payload: {
          source_run_id: 'run-interaction-atlas',
          source_interaction_id: 'interaction-atlas',
          status: 'inferred',
          matched_units: 7,
          matched_bytes: 283,
          input_start: 2,
          candidate_count: 1,
        },
      },
      {
        sequence: 14,
        occurred_at: startedAt + 299_300,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'future_observation_event',
        payload: { opaque: { values: [false, null, 0, 'uninterpreted payload'] }, status: 'future_status' },
      },
    ]
    for (const event of events) fixture.emit(event, 'Cinder client-visible answer')
    await page.goto('/logs')
    await node(page, 'Cinder', 'running').getByRole('heading', { name: 'Cinder', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    const diagnosticsTab = inspector.getByRole('tab', { name: 'Diagnostics', exact: true })
    await diagnosticsTab.click()
    const diagnostics = inspector.getByRole('tabpanel', { name: 'Diagnostics', exact: true })
    const rawButtons = diagnostics.getByRole('button', { name: 'Raw event data', exact: true })
    await expect(rawButtons).toHaveCount(6)
    await expect(rawButtons.nth(2).locator('xpath=ancestor::li[1]')).toContainText('Cinder')
    await expect(rawButtons.nth(4).locator('xpath=ancestor::li[1]')).toContainText(/inferred/i)
    await expect(diagnostics.locator('pre:visible')).toHaveCount(0)
    for (const value of [
      'run-interaction-cinder',
      'interaction-cinder',
      'generation-root-cinder',
      'generation-parent-atlas',
      'run-interaction-atlas',
      'interaction-atlas',
      routeIds.cinder,
      'uninterpreted payload',
    ]) {
      await expect(diagnostics.getByText(value, { exact: true })).toBeHidden()
    }
    for (const event of events) await expect(diagnostics.getByText(event.kind, { exact: true })).toBeHidden()

    // The two original run events precede these four records. Inspect each record
    // independently so an accidental shared disclosure cannot expose other payloads.
    for (const [index, event] of events.entries()) {
      const button = rawButtons.nth(index + 2)
      await button.focus()
      await page.keyboard.press('Enter')
      await expect(button).toHaveAttribute('aria-expanded', 'true')
      await expect(diagnostics.getByText(event.kind, { exact: true })).toBeVisible()
      const payload = diagnostics.locator('pre:visible')
      await expect(payload).toHaveCount(1)
      expect(JSON.parse(await payload.innerText())).toEqual(event.payload)
      await button.focus()
      await page.keyboard.press('Space')
      await expect(button).toHaveAttribute('aria-expanded', 'false')
      await expect(payload).toHaveCount(0)
    }

    for (const button of await diagnostics.getByRole('button', { name: 'Technical identifiers', exact: true }).all()) {
      await button.focus()
      await page.keyboard.press('Enter')
    }
    await expect(diagnostics.getByText('interaction-cinder', { exact: true })).toBeVisible()
    await expect(diagnostics.getByText('run-interaction-cinder', { exact: true })).toBeVisible()
    await inspector.getByRole('tab', { name: 'Conversation', exact: true }).click()
    await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText('Cinder client-visible answer')
    await inspector.getByRole('tab', { name: 'Debug records', exact: true }).click()
    await expect(inspector.getByText('retained diagnostic record', { exact: false })).toBeVisible()
    await diagnosticsTab.click()
    await expect(rawButtons).toHaveCount(6)

    const liveEvent: ObservationEvent = {
      ...events[3],
      sequence: 15,
      occurred_at: startedAt + 299_400,
      payload: { new_record: 'live raw payload', prior_sequence: 14 },
    }
    fixture.emit(liveEvent, 'Cinder client-visible answer')
    await expect(rawButtons).toHaveCount(7)
    await expect(diagnosticsTab).toHaveAttribute('aria-selected', 'true')
    await expect(diagnostics.locator('pre:visible')).toHaveCount(0)
    await rawButtons.last().focus()
    await page.keyboard.press('Enter')
    expect(JSON.parse(await diagnostics.locator('pre:visible').innerText())).toEqual(liveEvent.payload)
  })

  test('folds consecutive response updates without losing events or hiding intervening failures', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    const emit = (sequence: number, kind: string, payload: unknown) =>
      fixture.emit({
        sequence,
        occurred_at: startedAt + 299_000 + sequence,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind,
        payload,
      })
    for (let sequence = 11; sequence <= 13; sequence++) {
      emit(sequence, 'client_visible_content_delta', { text: `chunk-${sequence}` })
    }
    await page.goto('/logs?interaction=interaction-cinder')
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await inspector.getByRole('tab', { name: 'Diagnostics', exact: true }).click()
    const diagnostics = inspector.getByRole('tabpanel', { name: 'Diagnostics', exact: true })
    const groups = diagnostics.getByRole('button', { name: /^Response updates \(/ })
    const group = groups.first()
    await expect(groups).toHaveCount(1)
    await expect(group).toHaveAccessibleName('Response updates (3)')
    await expect(group).toHaveAttribute('aria-expanded', 'false')
    const records = group.locator('xpath=ancestor::li[1]').getByRole('button', { name: 'Raw event data', exact: true })
    await expect(records).toHaveCount(0)
    await group.focus()
    await page.keyboard.press('Enter')
    await expect(records).toHaveCount(3)

    emit(14, 'client_visible_content_delta', { text: 'chunk-14' })
    await expect(group).toHaveAccessibleName('Response updates (4)')
    await expect(group).toHaveAttribute('aria-expanded', 'true')
    await expect(records).toHaveCount(4)
    for (let index = 0; index < 4; index++) {
      await records.nth(index).click()
      expect(JSON.parse(await diagnostics.locator('pre:visible').innerText())).toEqual({ text: `chunk-${11 + index}` })
      await records.nth(index).click()
    }

    emit(15, 'delivery_finished', { status: 'delivery_failed', reason: 'client_disconnected' })
    emit(16, 'client_visible_content_delta', { text: 'after-failure-16' })
    emit(17, 'client_visible_content_delta', { text: 'after-failure-17' })
    await expect(groups).toHaveCount(2)
    await expect(group).toHaveAccessibleName('Response updates (4)')
    await expect(groups.last()).toHaveAccessibleName('Response updates (2)')
    await expect(diagnostics.getByText('client_disconnected', { exact: true })).toBeVisible()
    await group.focus()
    await page.keyboard.press('Space')
    await expect(records).toHaveCount(0)
    await expect(diagnostics.locator('pre:visible')).toHaveCount(0)
    await expect(diagnostics.getByText('client_disconnected', { exact: true })).toBeVisible()
  })

  test('orders related model events and client output by time, then sequence, without crossing intervening events', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    const rows: Array<[string, number, Record<string, unknown>]> = [
      ['model_turn_started', 0, { model_turn_id: 'ordered-turn', model_display_name: 'Cinder' }],
      ['target_attempt_started', 0, { model_turn_id: 'ordered-turn', attempt_id: 'ordered-attempt' }],
      ['client_output_committed', 5000, {}],
      ['client_visible_content_delta', 6000, { text: 'first streamed output' }],
      [
        'usage_confirmed',
        6500,
        { model_turn_id: 'ordered-turn', attempt_id: 'ordered-attempt', usage: { output_tokens: 214 } },
      ],
      [
        'target_attempt_finished',
        7000,
        { model_turn_id: 'ordered-turn', attempt_id: 'ordered-attempt', status: 'completed' },
      ],
      ['model_turn_finished', 7000, { model_turn_id: 'ordered-turn', status: 'completed' }],
      ['client_visible_content_delta', 6600, { text: 'output recorded before upstream completion' }],
    ]
    const events = rows.map(([kind, offset, payload], index): ObservationEvent => ({
      sequence: 11 + index,
      occurred_at: startedAt + 290_000 + offset,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind,
      payload,
    }))
    for (const event of events) fixture.emit(event)
    await page.goto('/logs?interaction=interaction-cinder')
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await inspector.getByRole('tab', { name: 'Diagnostics', exact: true }).click()
    const diagnostics = inspector.getByRole('tabpanel', { name: 'Diagnostics', exact: true })
    const groups = diagnostics.getByRole('button', { name: /^Response updates \(/ })
    for (const group of await groups.all()) await group.click()
    const observed: Array<{ kind: string; payload: unknown }> = []
    for (const button of await diagnostics.getByRole('button', { name: 'Raw event data', exact: true }).all()) {
      await button.click()
      observed.push({
        kind: await diagnostics.locator('.event-kind:visible code').innerText(),
        payload: JSON.parse(await diagnostics.locator('pre:visible').innerText()),
      })
      await button.click()
    }
    const expected = events
      .toSorted((a, b) => a.occurred_at - b.occurred_at || a.sequence - b.sequence)
      .map(({ kind, payload }) => ({ kind, payload }))
    expect(observed.slice(2)).toEqual(expected)
    await expect(groups).toHaveCount(0)
  })

  test('groups only consecutive same-name client tools and shows attempt-local output speed', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    const rows: Array<[string, Record<string, unknown>]> = [
      ['usage_confirmed', { attempt_id: 'other-attempt', usage: { output_tokens: 9999 } }],
      ['usage_confirmed', { attempt_id: 'current-attempt', usage: { output_tokens: 1110 } }],
      [
        'target_attempt_finished',
        { attempt_id: 'current-attempt', status: 'completed', duration_ms: 18750, first_token_ms: 7650 },
      ],
      ...Array.from({ length: 4 }, (_, index): [string, Record<string, unknown>] => [
        'client_tool_handoff',
        { name: 'Bash', tool_id: `bash-${index}` },
      ]),
      ['client_tool_handoff', { name: 'Read', tool_id: 'read-1' }],
      ['client_tool_handoff', { name: 'Bash', tool_id: 'bash-4' }],
      ['client_tool_handoff', { name: 'Bash', tool_id: 'bash-5' }],
    ]
    for (const [index, [kind, payload]] of rows.entries()) {
      fixture.emit({
        sequence: 11 + index,
        occurred_at: startedAt + 299_000 + index,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind,
        payload,
      })
    }
    await page.goto('/logs?interaction=interaction-cinder')
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await inspector.getByRole('tab', { name: 'Diagnostics', exact: true }).click()
    const diagnostics = inspector.getByRole('tabpanel', { name: 'Diagnostics', exact: true })
    await expect(diagnostics.getByText('100 tok/s', { exact: true })).toBeVisible()
    const groups = diagnostics.getByRole('button', { name: /^Bash ×/ })
    await expect(groups).toHaveCount(2)
    await expect(groups.first()).toHaveAccessibleName('Bash × 4 · Sent to client')
    await expect(groups.last()).toHaveAccessibleName('Bash × 2 · Sent to client')
    await expect(diagnostics.getByText('Read', { exact: true })).toBeVisible()
    const records = groups
      .first()
      .locator('xpath=ancestor::li[1]')
      .getByRole('button', { name: 'Raw event data', exact: true })
    await expect(records).toHaveCount(0)
    await groups.first().focus()
    await page.keyboard.press('Enter')
    await expect(records).toHaveCount(4)
    for (let index = 0; index < 4; index++) {
      await records.nth(index).click()
      expect(JSON.parse(await diagnostics.locator('pre:visible').innerText())).toEqual({
        name: 'Bash',
        tool_id: `bash-${index}`,
      })
      await records.nth(index).click()
    }
  })

  for (const captureDebug of [false, true]) {
    test(`expands ${captureDebug ? 'legacy Debug' : 'ordinary'} thinking and tool details without storing content`, async ({
      page,
    }) => {
      let completed = false
      const fixture = await installObservationFixture(page, false, captureDebug, false, (detail) => {
        if (detail.interaction.id !== 'interaction-cinder') return
        const parent = detail.runs[0]
        const trace = (runId: string, sequence: number, stage: string, payload: unknown) => ({
          schema_version: 1,
          sequence,
          recorded_at: startedAt + 299_000 + sequence,
          interaction_id: detail.interaction.id,
          run_id: runId,
          model_turn_id: 'activity-turn',
          attempt_id: stage === 'canonical_delta' ? 'activity-attempt' : null,
          layer: 'canonical',
          payload_encoding: 'json',
          stage,
          payload,
        })
        parent.status = 'waiting_client'
        parent.finished_at = startedAt + 299_005
        parent.events = [
          ...parent.events,
          {
            sequence: 11,
            occurred_at: startedAt + 299_003,
            interaction_id: detail.interaction.id,
            run_id: parent.id,
            rejection_id: null,
            kind: 'client_tool_handoff',
            payload: {
              tool_id: 'call-bash',
              name: 'Bash',
              ...(!captureDebug ? { input: { command: 'printf "tool-output-sentinel"' } } : {}),
            },
          },
        ]
        parent.debug_events = [
          trace(parent.id, 1, 'canonical_delta', { kind: 'thinking_delta', data: 'Inspect the environment. ' }),
          trace(parent.id, 2, 'canonical_delta', { kind: 'thinking_delta', data: 'Then run the command.' }),
          trace(parent.id, 3, 'response_after_hook', {
            items: [
              {
                role: 'assistant',
                content: [
                  {
                    type: 'thinking',
                    thinking: 'Inspect the environment. Then run the command.',
                    signature: 'opaque-signature-sentinel',
                  },
                  {
                    type: 'tool_use',
                    id: 'call-bash',
                    name: 'Bash',
                    input: { command: 'printf "tool-output-sentinel"' },
                  },
                ],
              },
            ],
          }),
        ]
        const ordinary = (sequence: number, kind: string, payload: unknown, runId = parent.id): ObservationEvent => ({
          sequence,
          kind,
          payload,
          run_id: runId,
          interaction_id: detail.interaction.id,
          rejection_id: null,
          occurred_at: startedAt + 299_000 + sequence,
        })
        if (!captureDebug) {
          parent.debug_events = []
          parent.events.push(
            ordinary(1, 'model_thinking_delta', {
              model_turn_id: 'activity-turn',
              attempt_id: 'activity-attempt',
              text: 'Inspect the environment. ',
            }),
            ordinary(2, 'model_thinking_delta', {
              model_turn_id: 'activity-turn',
              attempt_id: 'activity-attempt',
              text: 'Then run the command.',
            }),
          )
          if (completed)
            parent.events.push(
              ordinary(3, 'model_thinking_finished', {
                model_turn_id: 'activity-turn',
                attempt_id: 'activity-attempt',
              }),
              ordinary(4, 'platform_tool_started', {
                model_turn_id: 'activity-turn',
                tool_id: 'call-search',
                name: 'Web search',
                input: { query: 'fixture' },
              }),
              ordinary(5, 'platform_tool_finished', {
                model_turn_id: 'activity-turn',
                tool_id: 'call-search',
                status: 'completed',
                content: { answer: 'platform-output-sentinel' },
              }),
            )
        }
        if (!completed) {
          parent.status = 'running'
          parent.finished_at = null
          parent.events = parent.events.filter((event) => event.kind !== 'client_tool_handoff')
          parent.debug_events = parent.debug_events.slice(0, 2)
          return
        }
        const childId = 'run-cinder-tool-return'
        detail.runs.push({
          ...parent,
          id: childId,
          parent_run_id: parent.id,
          model_display_name: 'Cinder',
          started_at: startedAt + 299_006,
          status: 'running',
          finished_at: null,
          events: captureDebug
            ? []
            : [
                ordinary(
                  6,
                  'client_tool_result',
                  { tool_id: 'call-bash', content: 'tool-output-sentinel\n', is_error: false },
                  childId,
                ),
              ],
          debug_events: captureDebug
            ? [
                trace(childId, 6, 'decoded_request', {
                  items: [
                    {
                      role: 'user',
                      content: [
                        {
                          type: 'tool_result',
                          tool_use_id: 'call-bash',
                          content: 'tool-output-sentinel\n',
                          is_error: false,
                        },
                      ],
                    },
                  ],
                }),
              ]
            : [],
        })
      })
      await page.goto('/logs?interaction=interaction-cinder')
      if (!captureDebug) await expect(page.getByRole('tab', { name: 'Debug records', exact: true })).toHaveCount(0)
      const conversation = page.getByRole('log', { name: 'Conversation' })
      const thinking = conversation.getByRole('button', { name: /^Thinking(?:…)?$/ })
      const tool = conversation.getByRole('button', { name: 'Tool call Bash', exact: true })
      await expect(thinking).toHaveAttribute('aria-expanded', 'false')
      await expect(thinking).toHaveAccessibleName('Thinking…')
      await expect(tool).toHaveCount(0)
      await expect(
        conversation.getByText('Inspect the environment. Then run the command.', { exact: true }),
      ).toBeHidden()
      await thinking.press('Enter')
      await expect(thinking).toHaveAttribute('aria-expanded', 'true')
      await expect(
        conversation.getByText('Inspect the environment. Then run the command.', { exact: true }),
      ).toBeVisible()
      completed = true
      fixture.emit({
        sequence: 12,
        occurred_at: startedAt + 299_008,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: captureDebug ? 'checkpoint' : 'model_thinking_finished',
        payload: { stage: 'response_after_hook', model_turn_id: 'activity-turn', attempt_id: 'activity-attempt' },
      })
      await expect(thinking).toHaveAccessibleName('Thinking')
      await expect(thinking).toHaveAttribute('aria-expanded', 'true')
      await expect(tool).toHaveAttribute('aria-expanded', 'false')
      await expect(conversation.getByRole('article', { name: 'Cinder', exact: true })).toHaveCount(1)
      const responseTime = conversation.getByRole('article', { name: 'Cinder', exact: true }).locator('time')
      await expect(responseTime).toHaveCount(1)
      await expect(responseTime).toHaveAttribute('datetime', new Date(startedAt + 299_006).toISOString())
      expect(await page.evaluate(() => document.body.innerText)).not.toContain('opaque-signature-sentinel')
      await tool.press('Enter')
      await expect(conversation.getByRole('heading', { name: 'Tool input', exact: true })).toBeVisible()
      await expect(conversation.getByText('Client result', { exact: true })).toBeVisible()
      await expect(conversation.getByText(/printf/)).toBeVisible()
      await expect(conversation.getByText('tool-output-sentinel', { exact: true })).toBeVisible()
      if (!captureDebug) {
        const platform = conversation.getByRole('button', { name: 'Tool call Web search', exact: true })
        await expect(platform).toHaveAttribute('aria-expanded', 'false')
        await platform.press('Enter')
        await expect(conversation.getByText('Tool result', { exact: true })).toBeVisible()
        await expect(conversation.getByText(/platform-output-sentinel/)).toBeVisible()
      }
      await thinking.click()
      await expect(thinking).toHaveAttribute('aria-expanded', 'false')
      await page.reload()
      await expect(thinking).toHaveAttribute('aria-expanded', 'false')
      await expect(tool).toHaveAttribute('aria-expanded', 'true')
      await expect(conversation.getByText('Client result', { exact: true })).toBeVisible()
      expect(await page.evaluate(() => JSON.stringify({ ...localStorage }))).not.toContain('tool-output-sentinel')
    })
  }

  test('reveals only received live graphemes and respects paused reading and reduced motion', async ({ page }) => {
    const fixture = await installObservationFixture(page, false, false, true)
    await page.goto('/logs')
    await node(page, 'Cinder', 'running').getByRole('heading', { name: 'Cinder', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    const conversation = inspector.getByRole('log', { name: 'Conversation' })
    const responseActor = conversation.getByRole('article', { name: 'Cinder', exact: true })
    const response = responseActor.locator('.markdown-content')
    const initial = ''
    await expect(response).toBeHidden()
    await responseActor.evaluate((element) => {
      const samples: string[] = []
      Object.assign(window, { conversationSamples: samples })
      new MutationObserver(() =>
        samples.push(element.querySelector('.markdown-content')?.textContent?.trim() ?? ''),
      ).observe(element, { childList: true, subtree: true, characterData: true })
    })
    const delta = `New live text: ${'🙂 e\u0301 👩🏽‍💻 '.repeat(8)}Finished.`
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: delta },
    })
    await expect(response).toHaveText(initial + delta)
    const samples = await page.evaluate(
      () => (window as unknown as { conversationSamples: string[] }).conversationSamples,
    )
    const prefixes = new Set([initial])
    let prefix = initial
    for (const { segment } of new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(delta)) {
      prefix += segment
      prefixes.add(prefix.trim())
    }
    expect(samples.some((sample) => sample.length > initial.length && sample.length < prefix.length)).toBe(true)
    expect(samples.every((sample) => prefixes.has(sample))).toBe(true)

    await page.emulateMedia({ reducedMotion: 'reduce' })
    await conversation.evaluate((element) => {
      element.scrollTop = 0
      element.dispatchEvent(new Event('scroll'))
      ;(window as unknown as { conversationSamples: string[] }).conversationSamples.length = 0
    })
    const secondDelta = ' Additional received content.'.repeat(30)
    fixture.emit({
      sequence: 12,
      occurred_at: startedAt + 299_100,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: secondDelta },
    })
    await expect(response).toHaveText(initial + delta + secondDelta)
    expect(await conversation.evaluate((element) => element.scrollTop)).toBe(0)
    const reducedSamples = await page.evaluate(
      () => (window as unknown as { conversationSamples: string[] }).conversationSamples,
    )
    expect(reducedSamples).toContain(initial + delta + secondDelta)
    expect(
      reducedSamples.every((sample) => sample === initial + delta || sample === initial + delta + secondDelta),
    ).toBe(true)
    await inspector.getByRole('button', { name: 'New activity · Back to latest', exact: true }).click()
    await expect
      .poll(() => conversation.evaluate((element) => element.scrollHeight - element.clientHeight - element.scrollTop))
      .toBeLessThanOrEqual(2)
    await inspector.getByRole('tab', { name: 'Diagnostics', exact: true }).click()
    fixture.emit({
      sequence: 13,
      occurred_at: startedAt + 299_200,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'delivery_terminal',
      payload: { delivered: true },
    })
    await expect(
      inspector
        .getByRole('tabpanel', { name: 'Diagnostics', exact: true })
        .getByText('Completed', { exact: true })
        .first(),
    ).toBeVisible()
    await expect(inspector.getByRole('tab', { name: 'Diagnostics', exact: true })).toHaveAttribute(
      'aria-selected',
      'true',
    )
    await inspector.getByRole('tab', { name: 'Conversation', exact: true }).click()
    await expect(response).toHaveText(initial + delta + secondDelta)
  })

  test('renders streamed GFM tables in conversation and output previews without overflowing a narrow viewport', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page, false, false, true)
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/logs?interaction=interaction-cinder')
    const conversation = page.getByRole('log', { name: 'Conversation' })
    const response = conversation.getByRole('article', { name: 'Cinder', exact: true })
    const chunks = [
      '## 核心配置\n\n| 项目 | 详情 |\n',
      '| --- | --- |\n',
      '| 处理器 | **AMD 锐龙 9 8940HX** |\n| 内存 | 48GB DDR5 |\n',
    ]
    for (const [index, text] of chunks.entries()) {
      fixture.emit({
        sequence: 11 + index,
        occurred_at: startedAt + 299_000 + index,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'client_visible_content_delta',
        payload: { text },
      })
      if (index === 0) await expect(response.getByRole('heading', { name: '核心配置' })).toBeVisible()
      if (index === 1) await expect(response.getByRole('columnheader')).toHaveText(['项目', '详情'])
    }
    await expect(response.getByRole('cell')).toHaveText(['处理器', 'AMD 锐龙 9 8940HX', '内存', '48GB DDR5'])
    await expect(response.locator('td strong')).toHaveText('AMD 锐龙 9 8940HX')
    await page.setViewportSize({ width: 320, height: 850 })
    await expect(response.getByRole('table')).toBeVisible()
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(320)
    await page.setViewportSize({ width: 1280, height: 900 })
    await page.getByRole('button', { name: 'Close', exact: true }).click()
    const preview = node(page, 'Cinder', 'running').getByRole('button', { name: 'Model output preview', exact: true })
    await preview.focus()
    const tooltip = page.getByRole('tooltip')
    await expect(tooltip.getByRole('columnheader')).toHaveText(['项目', '详情'])
    await expect(tooltip.getByRole('cell')).toHaveText(['处理器', 'AMD 锐龙 9 8940HX', '内存', '48GB DDR5'])
  })

  test('renders safe Markdown and keeps the newest output line inside the card', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    const card = node(page, 'Cinder', 'running').locator('article')
    await expect(card).toBeVisible()

    for (const sequence of [11, 12]) {
      const tail = [
        '# Answer',
        ...Array.from({ length: 12 }, (_, index) => `Paragraph ${index}: **bold** and \`code\`.`),
        '<img src="https://example.invalid/tracker" onerror="window.markdownExecuted=true">',
        '<script>window.markdownExecuted=true</script>',
        '[unsafe](javascript:alert(1))',
        `**Latest ${sequence}** with \`code\``,
      ].join('\n\n')
      fixture.emit(
        {
          sequence,
          occurred_at: startedAt + 299_000 + sequence,
          interaction_id: 'interaction-cinder',
          run_id: 'run-interaction-cinder',
          rejection_id: null,
          kind: 'client_projection_delta',
          payload: { text: tail },
        },
        tail,
      )
      const latest = card.locator('.tail-preview .markdown-content strong').filter({ hasText: `Latest ${sequence}` })
      await expect(latest).toBeVisible()
      await expect
        .poll(async () =>
          latest.evaluate((element) => {
            const line = element.getBoundingClientRect()
            const viewport = element.closest('.tail-preview')!.getBoundingClientRect()
            return line.top >= viewport.top && line.bottom <= viewport.bottom + 1
          }),
        )
        .toBe(true)
      await expect(card.locator('.tail-preview .markdown-content code').last()).toHaveText('code')
      await expect(card.locator('img, script, [onerror], a[href^="javascript:"]')).toHaveCount(0)
      expect(await page.evaluate(() => 'markdownExecuted' in window)).toBe(false)
    }
  })

  test('input and recent output previews expand without opening details and remain readable outside the scaled canvas', async ({
    page,
  }) => {
    await installObservationFixture(page)
    await page.goto('/logs')
    const card = node(page, 'Atlas', 'completed')
    const input = card.getByRole('button', { name: 'User input preview', exact: true })
    const output = card.getByRole('button', { name: 'Model output preview', exact: true })
    await expect(input.locator('strong')).toHaveText('Atlas user question')
    await expect(output).toContainText('Atlas client-visible answer')
    await expect(input).not.toContainText('Atlas client-visible answer')
    await expect(
      node(page, 'Boreal', 'waiting_client').getByRole('button', { name: 'User input preview' }),
    ).toContainText('No text input preview recorded.')
    await input.hover()
    const tooltip = page.getByRole('tooltip')
    await expect(tooltip).toBeVisible()
    await expect(tooltip.locator('code')).toHaveText('Markdown')
    await tooltip.hover()
    await expect(tooltip).toBeVisible()
    expect(await tooltip.evaluate((element) => element.closest('.svelte-flow__viewport') === null)).toBe(true)
    const scrollRegion = tooltip.getByRole('region')
    await scrollRegion.evaluate((element) => {
      element.scrollTop = element.scrollHeight
    })
    await expect(tooltip.getByText('Input preview ending', { exact: true })).toBeInViewport()
    await page.keyboard.press('Escape')
    await expect(tooltip).toBeHidden()
    await output.focus()
    await expect(tooltip).toContainText('Atlas client-visible answer')
    await output.press('Enter')
    await expect(tooltip).toBeVisible()
    await expect(page.getByRole('complementary', { name: 'Observation details' })).toBeHidden()
    await page.keyboard.press('Escape')
    await expect(tooltip).toBeHidden()
    await input.focus()
    await input.press('ArrowDown')
    await expect(tooltip.getByRole('region')).toBeFocused()
    await tooltip.getByRole('region').press('End')
    await expect(tooltip.getByText('Input preview ending', { exact: true })).toBeInViewport()
    await page.keyboard.press('Escape')
    await expect(tooltip).toBeHidden()
    await page.getByRole('button', { name: 'Enter fullscreen', exact: true }).click()
    await input.hover()
    await expect(tooltip).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(tooltip).toBeHidden()
    await expect(page.getByRole('button', { name: 'Exit fullscreen', exact: true })).toBeVisible()
    await page.getByRole('button', { name: 'Exit fullscreen', exact: true }).click()
    await card.getByRole('heading', { name: 'Atlas', exact: true }).click()
    await expect(page.getByRole('complementary', { name: 'Observation details' })).toBeVisible()
  })

  test('a credential discovery link opens its exact observation outside the loaded root batch', async ({ page }) => {
    await installObservationFixture(page, true)
    await page.goto('/logs?interaction=interaction-ember')
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector.getByRole('heading', { name: 'Ember', exact: true, level: 2 })).toBeVisible()
    await expect(inspector.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toHaveCount(0)
    await page.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(inspector).toBeHidden()
  })

  test('downloads the bundle as a file without entering the application router', async ({ page }) => {
    await installObservationFixture(page)
    await page.route('**/api/v1/observations/interactions/interaction-atlas/debug-bundle-tickets', (route) =>
      route.fulfill({
        json: {
          data: {
            download_url: '/api/v1/observations/debug-bundles/browser-export-fixture',
            expires_at: Date.now() + 60_000,
            through_sequence: 10,
          },
        },
      }),
    )
    const emptyZip = Buffer.alloc(22)
    emptyZip.writeUInt32LE(0x06054b50)
    await page.route('**/api/v1/observations/debug-bundles/browser-export-fixture', (route) =>
      route.fulfill({
        contentType: 'application/zip',
        headers: { 'content-disposition': 'attachment; filename="observation.zip"', 'cache-control': 'no-store' },
        body: emptyZip,
      }),
    )
    await page.goto('/logs')
    await node(page, 'Atlas', 'completed').getByRole('heading', { name: 'Atlas', exact: true }).click()
    const download = page.waitForEvent('download')
    await page.getByRole('button', { name: 'Debug bundle', exact: true }).click()
    const completed = await download
    expect(completed.suggestedFilename()).toBe('observation.zip')
    expect(await completed.failure()).toBeNull()
    await expect(page).toHaveURL('/logs')
  })

  test('lays out complete root trees and keeps pointer, keyboard, inspector, and overview controls operable', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
    const fixture = await installObservationFixture(page, true)
    await page.goto('/logs')

    await expect(page.getByText('1 / 3 roots', { exact: true })).toBeVisible()
    await expect.poll(() => fixture.forestRequests.some((url) => url.searchParams.has('cursor'))).toBe(true)
    await page.getByRole('button', { name: 'Load and fit all roots' }).click()
    await expect(page.getByRole('progressbar', { name: 'Loading all roots before fitting' })).toBeVisible()
    fixture.releaseRemainingRoots()
    await expect(page.getByText('3 / 3 roots', { exact: true })).toBeVisible()
    await expect(node(page, 'Atlas', 'completed')).toHaveCount(1)

    await expect(async () => {
      const [atlasBox, borealBox, cinderBox, deltaBox] = await page.evaluate(
        (labels) =>
          labels.map((label) => {
            const element = document.querySelector(`.svelte-flow__node[aria-label="${label}"]`)
            return element?.getBoundingClientRect().toJSON() as DOMRect | undefined
          }),
        ['Atlas, Completed', 'Boreal, Waiting for client', 'Cinder, Running', 'Delta, Completed'],
      )
      expect(atlasBox).toBeDefined()
      expect(borealBox).toBeDefined()
      expect(cinderBox).toBeDefined()
      expect(deltaBox).toBeDefined()
      expect(atlasBox!.y).toBeLessThan(borealBox!.y)
      expect(Math.abs(borealBox!.y - cinderBox!.y)).toBeLessThan(8)
      expect(Math.abs(borealBox!.x - cinderBox!.x)).toBeGreaterThan(35)
      expect(deltaBox!.x).toBeGreaterThan(Math.max(borealBox!.right, cinderBox!.right))
    }).toPass({ timeout: 5_000 })

    await node(page, 'Atlas', 'completed').getByRole('heading', { name: 'Atlas', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector.getByRole('heading', { name: 'Atlas', level: 2 })).toBeVisible()
    const widthBefore = (await inspector.boundingBox())!.width
    const resize = page.getByRole('slider', { name: 'Resize details inspector' })
    await resize.press('ArrowRight')
    await resize.press('ArrowRight')
    await expect.poll(async () => (await inspector.boundingBox())!.width).toBeGreaterThan(widthBefore)
    const widthBeforeDrag = (await inspector.boundingBox())!.width
    const handleBox = (await resize.boundingBox())!
    await page.mouse.move(handleBox.x + handleBox.width / 2, handleBox.y + handleBox.height / 2)
    await page.mouse.down()
    await page.mouse.move(handleBox.x - 30, handleBox.y + handleBox.height / 2)
    await page.mouse.up()
    await expect.poll(async () => (await inspector.boundingBox())!.width).toBeGreaterThan(widthBeforeDrag)
    await page.getByRole('button', { name: 'Close', exact: true }).click()

    await node(page, 'Boreal', 'waiting_client').focus()
    await node(page, 'Boreal', 'waiting_client').press('Enter')
    await expect(inspector.getByRole('heading', { name: 'Boreal', level: 2 })).toBeVisible()
    await page.getByRole('button', { name: 'Close', exact: true }).click()

    await page.getByRole('button', { name: 'Show minimap' }).click()
    await expect(page.getByLabel('Interaction forest minimap')).toBeVisible()
    await page.getByRole('button', { name: 'Hide minimap' }).click()
    await expect(page.getByLabel('Interaction forest minimap')).toHaveCount(0)

    const pane = page.locator('.svelte-flow__pane')
    const paneBox = (await pane.boundingBox())!
    const pointer = { x: paneBox.x + paneBox.width * 0.58, y: paneBox.y + paneBox.height * 0.52 }
    const beforeZoom = await viewportTransform(page)
    const localPointer = { x: pointer.x - paneBox.x, y: pointer.y - paneBox.y }
    const worldUnderPointer = {
      x: (localPointer.x - beforeZoom.x) / beforeZoom.zoom,
      y: (localPointer.y - beforeZoom.y) / beforeZoom.zoom,
    }
    await page.mouse.move(pointer.x, pointer.y)
    await page.mouse.wheel(0, -260)
    await expect.poll(async () => (await viewportTransform(page)).zoom).toBeGreaterThan(beforeZoom.zoom)
    const afterZoom = await viewportTransform(page)
    expect(Math.abs(worldUnderPointer.x * afterZoom.zoom + afterZoom.x - localPointer.x)).toBeLessThan(6)
    expect(Math.abs(worldUnderPointer.y * afterZoom.zoom + afterZoom.y - localPointer.y)).toBeLessThan(6)

    await page.mouse.move(pointer.x, pointer.y)
    await page.mouse.down()
    await page.mouse.move(pointer.x + 90, pointer.y + 45, { steps: 5 })
    await page.mouse.up()
    await expect.poll(async () => (await viewportTransform(page)).x).not.toBe(afterZoom.x)
  })

  test('preserves causal roots through filters while live activity pauses and resumes follow', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')

    await node(page, 'Atlas', 'completed').getByRole('heading', { name: 'Atlas', exact: true }).click()
    await page.getByRole('button', { name: 'Close', exact: true }).click()
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_projection_delta',
      payload: { text: 'new visible activity' },
    })
    const follow = page.getByRole('button', { name: 'New activity · Follow' })
    await expect(follow).toBeVisible({ timeout: 5_000 })
    await follow.click()
    await expect(follow).toHaveCount(0)

    await page.getByRole('button', { name: 'Filters' }).click()
    await page.getByLabel('Status', { exact: true }).click()
    await page.getByRole('option', { name: 'Completed', exact: true }).click()
    await page.getByRole('button', { name: 'Apply filters' }).click()
    await expect(node(page, 'Atlas', 'completed')).toBeVisible()
    await expect(node(page, 'Boreal', 'waiting_client')).toBeVisible()
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    await expect(node(page, 'Atlas', 'completed').locator('article')).toHaveCSS('opacity', '1')
    await expect(node(page, 'Boreal', 'waiting_client').locator('article')).toHaveCSS('opacity', '0.42')
    await expect(node(page, 'Cinder', 'running').locator('article')).toHaveCSS('opacity', '0.42')

    expect(
      fixture.forestRequests.every((url) => url.searchParams.has('start_at') && url.searchParams.has('end_at')),
    ).toBe(true)
  })

  test('fullscreen preserves the selected interaction and exits by button or Escape', async ({ page }) => {
    await installObservationFixture(page)
    await page.goto('/logs')
    await node(page, 'Atlas', 'completed').getByRole('heading', { name: 'Atlas', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await page.getByRole('button', { name: 'Enter fullscreen', exact: true }).click()
    await expect(inspector.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await page.getByRole('button', { name: 'Choose date and time range', exact: true }).click()
    const rangeDialog = page.getByRole('dialog', { name: 'Date and time range', exact: true })
    await expect(rangeDialog.getByLabel('Start time', { exact: true })).toBeVisible()
    await rangeDialog.getByRole('button', { name: 'Cancel', exact: true }).click()
    await page.getByRole('button', { name: 'Exit fullscreen', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Enter fullscreen', exact: true })).toBeVisible()
    await page.getByRole('button', { name: 'Enter fullscreen', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Exit fullscreen', exact: true })).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('button', { name: 'Enter fullscreen', exact: true })).toBeVisible()
    await expect(inspector.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await inspector.getByRole('button', { name: 'Close', exact: true }).click()
    await page.setViewportSize({ width: 390, height: 740 })
    await page.getByRole('button', { name: 'Enter fullscreen', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Exit fullscreen', exact: true })).toBeVisible()
    await node(page, 'Atlas', 'completed').focus()
    await node(page, 'Atlas', 'completed').press('Enter')
    const mobileInspector = page.getByRole('dialog', { name: 'Observation details' })
    await expect(mobileInspector.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await mobileInspector.getByRole('button', { name: 'Close', exact: true }).click()
    await page.getByRole('button', { name: 'Exit fullscreen', exact: true }).click()
  })

  test('presets send bounded durations and custom local ranges reject more than 24 hours', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    const presets = [
      ['5 minutes', 300_000],
      ['10 minutes', 600_000],
      ['30 minutes', 1_800_000],
      ['1 hour', 3_600_000],
      ['4 hours', 14_400_000],
      ['12 hours', 43_200_000],
      ['24 hours', DAY],
    ] as const
    for (const [label, duration] of presets) {
      await page.getByRole('button', { name: 'Time window', exact: true }).click()
      await page.getByRole('option', { name: label, exact: true }).click()
      await expect
        .poll(() => {
          const params = fixture.forestRequests.at(-1)?.searchParams
          return Number(params?.get('end_at')) - Number(params?.get('start_at'))
        })
        .toBe(duration)
    }
    await page.getByRole('button', { name: 'Choose date and time range', exact: true }).click()
    const dialog = page.getByRole('dialog', { name: 'Date and time range', exact: true })
    const start = '2026-09-06T08:00'
    const end = '2026-09-07T08:00'
    await dialog.getByLabel('Start time', { exact: true }).fill(start)
    await dialog.getByLabel('End time', { exact: true }).fill('2026-09-07T08:01')
    await expect(dialog.getByRole('button', { name: 'Apply', exact: true })).toBeDisabled()
    await dialog.getByLabel('End time', { exact: true }).fill(end)
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click()
    await expect(dialog).toBeHidden()
    const expected = await page.evaluate(
      ([start, end]) => [new Date(start).getTime(), new Date(end).getTime()],
      [start, end],
    )
    await expect
      .poll(() => {
        const params = fixture.forestRequests.at(-1)?.searchParams
        return [Number(params?.get('start_at')), Number(params?.get('end_at'))]
      })
      .toEqual(expected)
  })

  test('live presets expire old roots while an applied custom range stays fixed', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    await page.getByRole('button', { name: 'Time window', exact: true }).click()
    await page.getByRole('option', { name: '5 minutes', exact: true }).click()
    await expect(node(page, 'Atlas', 'completed')).toBeVisible()
    await page.evaluate((now) => (Date.now = () => now), startedAt + 900_000)
    await expect(node(page, 'Atlas', 'completed')).toHaveCount(0)
    await expect
      .poll(() => {
        const params = fixture.forestRequests.at(-1)?.searchParams
        return Number(params?.get('end_at')) - Number(params?.get('start_at'))
      })
      .toBe(300_000)

    await page.getByRole('button', { name: 'Choose date and time range', exact: true }).click()
    const dialog = page.getByRole('dialog', { name: 'Date and time range', exact: true })
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click()
    await expect(dialog).toBeHidden()
    const fixed = fixture.forestRequests.at(-1)!.searchParams
    await page.evaluate((now) => (Date.now = () => now), startedAt + DAY * 2)
    await page.getByRole('button', { name: 'Refresh and anchor a new current window' }).click()
    await expect
      .poll(() => {
        const params = fixture.forestRequests.at(-1)!.searchParams
        return [params.get('start_at'), params.get('end_at')]
      })
      .toEqual([fixed.get('start_at'), fixed.get('end_at')])
  })

  test('retains the selected observation and detail tab across mobile and desktop layouts', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 })
    await installObservationFixture(page, false, true)
    await page.goto('/logs')
    const selectedNode = node(page, 'Atlas', 'completed')
    await selectedNode.focus()
    await selectedNode.press('Enter')
    const desktop = page.getByRole('complementary', { name: 'Observation details' })
    await desktop.getByRole('tab', { name: 'Debug records' }).click()
    await expect(desktop.getByText('retained diagnostic record', { exact: false })).toBeVisible()

    await page.setViewportSize({ width: 390, height: 740 })
    const mobile = page.getByRole('dialog', { name: 'Observation details' })
    await expect(mobile.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await expect(mobile.getByRole('tab', { name: 'Debug records' })).toHaveAttribute('aria-selected', 'true')
    await expect(mobile.getByText('retained diagnostic record', { exact: false })).toBeVisible()
    await expect(desktop).toBeHidden()

    await page.setViewportSize({ width: 1280, height: 800 })
    await expect(desktop.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await expect(desktop.getByRole('tab', { name: 'Debug records' })).toHaveAttribute('aria-selected', 'true')
    await expect(desktop.getByText('retained diagnostic record', { exact: false })).toBeVisible()
    await expect(mobile).toBeHidden()
    await desktop.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(selectedNode).toBeFocused()
  })

  test.describe('touch input', () => {
    test.use({ hasTouch: true })

    test('tapping either preview opens bounded scrollable content without opening the inspector', async ({ page }) => {
      await page.setViewportSize({ width: 390, height: 740 })
      await installObservationFixture(page)
      await page.goto('/logs')
      const card = node(page, 'Atlas', 'completed')
      const tooltip = page.getByRole('tooltip')
      for (const [label, text] of [
        ['User input preview', 'Atlas user question'],
        ['Model output preview', 'Atlas client-visible answer'],
      ]) {
        await card.getByRole('button', { name: label, exact: true }).tap()
        await expect(tooltip).toContainText(text)
        await expect(page.getByRole('dialog', { name: 'Observation details' })).toBeHidden()
        const box = (await tooltip.boundingBox())!
        expect(box.x).toBeGreaterThanOrEqual(0)
        expect(box.x + box.width).toBeLessThanOrEqual(391)
        expect(box.y).toBeGreaterThanOrEqual(0)
        expect(box.y + box.height).toBeLessThanOrEqual(741)
        await page.locator('h1').tap()
        await expect(tooltip).toBeHidden()
      }
      await card.getByRole('button', { name: 'User input preview', exact: true }).tap()
      await page.keyboard.press('Escape')
      await expect(tooltip).toBeHidden()
      await expect(page.getByRole('dialog', { name: 'Observation details' })).toBeHidden()
    })

    test('uses a full-screen touch inspector, confirms every Debug enable, and honors reduced motion', async ({
      page,
    }) => {
      await page.setViewportSize({ width: 390, height: 740 })
      await page.emulateMedia({ reducedMotion: 'reduce' })
      const fixture = await installObservationFixture(page)
      await page.goto('/logs')

      const runningDot = node(page, 'Cinder', 'running').locator('[data-status="running"] .status-dot')
      const waitingDot = node(page, 'Boreal', 'waiting_client').locator('[data-status="waiting_client"] .status-dot')
      await expect(runningDot).toHaveCSS('animation-name', 'none')
      await expect(waitingDot).toHaveCSS('animation-name', 'none')

      const selectedNode = node(page, 'Cinder', 'running')
      await selectedNode.focus()
      await selectedNode.press('Enter')
      const inspector = page.getByRole('dialog', { name: 'Observation details' })
      await expect(inspector.getByRole('heading', { name: 'Cinder', level: 2 })).toBeVisible()
      const inspectorBox = (await inspector.boundingBox())!
      expect(inspectorBox.x).toBeLessThanOrEqual(1)
      expect(inspectorBox.width).toBeGreaterThanOrEqual(389)
      await expect(page.getByRole('slider', { name: 'Resize details inspector' })).toBeHidden()
      const close = inspector.getByRole('button', { name: 'Close', exact: true })
      await expect(close).toBeFocused()
      await close.press('Shift+Tab')
      await expect(close).not.toBeFocused()
      await expect.poll(() => inspector.evaluate((element) => element.contains(document.activeElement))).toBe(true)
      await page.keyboard.press('Tab')
      await expect(close).toBeFocused()
      await page.keyboard.press('Escape')
      await expect(inspector).toBeHidden()
      await expect(selectedNode).toBeFocused()
      await selectedNode.press('Enter')
      await close.click()
      await expect(inspector).toBeHidden()
      await expect(selectedNode).toBeFocused()

      const debugSwitch = page.getByRole('switch', { name: 'Debug' })
      await debugSwitch.click()
      const confirmation = page.getByRole('alertdialog', { name: 'Enable Debug' })
      await expect(confirmation).toContainText('Confirm every enable action.')
      await confirmation.getByRole('button', { name: 'Cancel' }).click()
      expect(fixture.debugWrites).toHaveLength(0)

      await debugSwitch.click()
      await page
        .getByRole('alertdialog', { name: 'Enable Debug' })
        .getByRole('button', { name: 'Enable Debug' })
        .click()
      await expect(debugSwitch).toBeChecked()
      expect(fixture.debugWrites).toEqual([{ enabled: true, confirmed: true }])
      await debugSwitch.click()
      await expect(debugSwitch).not.toBeChecked()
      expect(fixture.debugWrites.at(-1)).toEqual({ enabled: false, confirmed: false })
      await debugSwitch.click()
      await expect(page.getByRole('alertdialog', { name: 'Enable Debug' })).toBeVisible()

      await page.getByRole('alertdialog', { name: 'Enable Debug' }).getByRole('button', { name: 'Cancel' }).click()
      await page.locator('.svelte-flow__pane').scrollIntoViewIfNeeded()
      const paneBox = (await page.locator('.svelte-flow__pane').boundingBox())!
      const centerX = paneBox.x + paneBox.width / 2
      const centerY = paneBox.y + paneBox.height / 2
      const beforePinch = await viewportTransform(page)
      const session = await page.context().newCDPSession(page)
      await session.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 2 })
      await session.send('Input.dispatchTouchEvent', {
        type: 'touchStart',
        touchPoints: [
          { x: centerX - 25, y: centerY, id: 0 },
          { x: centerX + 25, y: centerY, id: 1 },
        ],
      })
      await session.send('Input.dispatchTouchEvent', {
        type: 'touchMove',
        touchPoints: [
          { x: centerX - 50, y: centerY, id: 0 },
          { x: centerX + 50, y: centerY, id: 1 },
        ],
      })
      await session.send('Input.dispatchTouchEvent', {
        type: 'touchMove',
        touchPoints: [
          { x: centerX - 75, y: centerY, id: 0 },
          { x: centerX + 75, y: centerY, id: 1 },
        ],
      })
      await session.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] })
      await expect.poll(async () => (await viewportTransform(page)).zoom).not.toBe(beforePinch.zoom)
      await session.detach()
    })
  })
})
