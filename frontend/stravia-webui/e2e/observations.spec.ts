import { expect, test, type Locator, type Page, type Route } from '@playwright/test'

import type {
  ConfirmedUsage,
  FailedRequestSummary,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  ObservationEvent,
  RunDetail,
  TraceManifest,
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
  input_tokens: 920,
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
  }
}

function failedRequest(
  id: string,
  kind: FailedRequestSummary['kind'],
  occurredAt: number,
  overrides: Partial<FailedRequestSummary> = {},
): FailedRequestSummary {
  return {
    id,
    kind,
    request_id: `req-${id}`,
    started_at: occurredAt,
    duration_ms: null,
    api_key_id: null,
    api_key_name: null,
    client: null,
    model: null,
    model_display_name: null,
    services: [],
    error: { source: null, code: null, message: null, status_code: null },
    interaction_id: null,
    root_id: null,
    run_id: null,
    debug_status: 'none',
    observation_gap: false,
    ...overrides,
  }
}

const logTime = (timestamp: number) =>
  `${new Intl.DateTimeFormat('en-US', { month: 'numeric', day: 'numeric', year: 'numeric' }).format(new Date(timestamp))} ${new Intl.DateTimeFormat('en-US', { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' }).format(new Date(timestamp))}`

// Only minute-precise offsets survive the round trip through a datetime-local input.
const localRangeValue = (offset: number) => {
  const date = new Date(startedAt + offset)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

interface ObservationFixture {
  emit(event: ObservationEvent, visibleTail?: string): void
  releaseRemainingRoots(): void
  holdNextRead(id: string): () => void
  addInteraction(item: InteractionSummary): void
  addFailure(
    request: FailedRequestSummary,
    detail?: { events?: ObservationEvent[]; trace?: TraceManifest | null },
  ): void
  holdNextFailureRead(): () => void
  reset(): void
  forestRequests: URL[]
  summaryRequests: URL[]
  detailRequests: URL[]
  eventRequests: URL[]
  failureRequests: URL[]
  failureDetailRequests: URL[]
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
  const summaryRequests: URL[] = []
  const detailRequests: URL[] = []
  const eventRequests: URL[] = []
  const heldDetails = new Map<string, Promise<void>>()
  let resetRequired = false
  const debugWrites: Array<{ enabled: boolean; confirmed: boolean }> = []
  const streamEvents: ObservationEvent[] = []
  let debugEnabled = false
  let snapshotSequence = 10
  const failures: FailedRequestSummary[] = []
  const failureDetails = new Map<string, { events: ObservationEvent[]; trace: TraceManifest | null }>()
  const failureRequests: URL[] = []
  const failureDetailRequests: URL[] = []
  let heldFailureList: Promise<void> | undefined

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
      if (resetRequired) {
        resetRequired = false
        streamEvents.length = 0
        await route.fulfill({
          contentType: 'text/event-stream',
          body: `event: reset_required\ndata: ${JSON.stringify({ snapshot_sequence: snapshotSequence })}\n\n`,
        })
        return
      }
      const event = streamEvents.find((item) => item.sequence > Number(url.searchParams.get('after')))
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

    if (path === '/observations/failed-requests') {
      failureRequests.push(url)
      const heldList = heldFailureList
      heldFailureList = undefined
      if (heldList) await heldList
      const start = Number(url.searchParams.get('start_at'))
      const end = Number(url.searchParams.get('end_at'))
      const limit = Number(url.searchParams.get('limit') ?? 30)
      const cursor = url.searchParams.get('cursor')
      const sorted = failures
        .filter((item) => item.started_at >= start && item.started_at < end)
        .toSorted((a, b) => b.started_at - a.started_at || a.kind.localeCompare(b.kind) || a.id.localeCompare(b.id))
      let offset = 0
      if (cursor) {
        const marker = JSON.parse(cursor) as { kind: string; id: string }
        offset = sorted.findIndex((item) => item.kind === marker.kind && item.id === marker.id) + 1
      }
      const items = sorted.slice(offset, offset + limit)
      const last = items.at(-1)
      await route.fulfill({
        json: {
          data: {
            items,
            total: sorted.length,
            next_cursor:
              last && offset + limit < sorted.length
                ? JSON.stringify({ started_at: last.started_at, kind: last.kind, id: last.id })
                : null,
            snapshot_sequence: snapshotSequence,
          },
        },
      })
      return
    }

    const failureDetailMatch = path.match(/^\/observations\/failed-requests\/([^/]+)\/([^/]+)$/)
    if (failureDetailMatch) {
      failureDetailRequests.push(url)
      const kind = decodeURIComponent(failureDetailMatch[1])
      const id = decodeURIComponent(failureDetailMatch[2])
      const matched = failures.find((item) => item.kind === kind && item.id === id)
      if (!matched) {
        await route.fulfill({ status: 404, json: { error: 'Failed request not found' } })
        return
      }
      const detail = failureDetails.get(`${kind}:${id}`)
      await route.fulfill({
        json: {
          data: {
            request: matched,
            events: detail?.events ?? [],
            trace: detail?.trace ?? null,
            snapshot_sequence: snapshotSequence,
          },
        },
      })
      return
    }

    const summaryMatch = path.match(/^\/observations\/interactions\/([^/]+)\/summary$/)
    if (summaryMatch) {
      summaryRequests.push(url)
      const id = decodeURIComponent(summaryMatch[1])
      const root = rootFor(id, url.searchParams.get('status'))
      await route.fulfill({
        json: {
          data: {
            interaction: root.interactions.find((item) => item.id === id)!,
            root,
            snapshot_sequence: snapshotSequence,
          },
        },
      })
      return
    }

    const detailMatch = path.match(/^\/observations\/interactions\/([^/]+)(\/events)?$/)
    if (detailMatch) {
      const eventsOnly = Boolean(detailMatch[2])
      if (eventsOnly) eventRequests.push(url)
      else detailRequests.push(url)
      const id = decodeURIComponent(detailMatch[1])
      const status = url.searchParams.get('status')
      const root = rootFor(id, status)
      const selected = root.interactions.find((item) => item.id === id)!
      const detail: InteractionDetail = {
        interaction: selected,
        root,
        runs: [{ ...recordedRuns.get(selected.id)!, debug_enabled: captureDebug }],
        snapshot_sequence: snapshotSequence,
        older_events_cursor: null,
      }
      transformDetail?.(detail)
      snapshotSequence = Math.max(
        snapshotSequence,
        ...detail.runs.flatMap((run) => run.events.map((event) => event.sequence)),
      )
      detail.snapshot_sequence = snapshotSequence
      const through = Number(url.searchParams.get('through_sequence') ?? snapshotSequence)
      const before = url.searchParams.get('before_sequence')
      const after = url.searchParams.get('after_sequence')
      const limit = Number(url.searchParams.get('limit') ?? 200)
      const candidates = detail.runs
        .flatMap((run) => run.events)
        .filter(
          (event) =>
            event.sequence <= through &&
            (before === null || event.sequence < Number(before)) &&
            (after === null || event.sequence > Number(after)),
        )
        .sort((a, b) => a.sequence - b.sequence)
      const chosen = after === null ? candidates.slice(-limit) : candidates.slice(0, limit)
      const sequences = new Set(chosen.map((event) => event.sequence))
      const cursor =
        candidates.length > chosen.length ? (after === null ? chosen[0].sequence : chosen.at(-1)!.sequence) : null
      detail.runs = detail.runs.map((run) => ({
        ...run,
        events: run.events.filter((event) => sequences.has(event.sequence)),
      }))
      detail.older_events_cursor = cursor
      const data = eventsOnly ? { runs: detail.runs, snapshot_sequence: through, next_cursor: cursor } : detail
      const body = JSON.stringify({ data })
      const held = heldDetails.get(id)
      heldDetails.delete(id)
      if (held) await held
      await route.fulfill({ contentType: 'application/json', body })
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
    holdNextRead(id) {
      let release!: () => void
      heldDetails.set(
        id,
        new Promise<void>((resolve) => {
          release = resolve
        }),
      )
      return release
    },
    addInteraction(item) {
      const root = roots.find((candidate) => candidate.id === item.root_id)
      if (root) {
        root.interactions.push(item)
        root.last_active_at = Math.max(root.last_active_at, item.last_active_at)
      } else roots.push({ id: item.root_id, last_active_at: item.last_active_at, interactions: [item] })
      recordedRuns.set(item.id, runFor(item))
    },
    addFailure(request, detail) {
      failures.push(request)
      failureDetails.set(`${request.kind}:${request.id}`, {
        events: detail?.events ?? [],
        trace: detail?.trace ?? null,
      })
    },
    holdNextFailureRead() {
      let release!: () => void
      heldFailureList = new Promise<void>((resolve) => {
        release = resolve
      })
      return release
    },
    reset() {
      resetRequired = true
    },
    emit(event, visibleTail) {
      snapshotSequence = Math.max(snapshotSequence, event.sequence)
      const known = roots.flatMap((root) => root.interactions).find((item) => item.id === event.interaction_id)
      if (known) {
        known.last_event_sequence = event.sequence
        known.last_active_at = event.occurred_at
        const root = roots.find((item) => item.id === known.root_id)!
        root.last_active_at = Math.max(...root.interactions.map((item) => item.last_active_at))
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
    summaryRequests,
    detailRequests,
    eventRequests,
    failureRequests,
    failureDetailRequests,
    debugWrites,
  }
}

async function installPersistentObservationStream(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const original = window.fetch.bind(window)
    window.fetch = async (input, init) => {
      const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
      if (!url.includes('/observations/events?')) return original(input, init)
      let remove = () => {}
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          const encoder = new TextEncoder()
          const receive = (event: Event) => {
            if (!(event instanceof CustomEvent)) return
            if (event.detail === 'disconnect') {
              remove()
              controller.close()
            } else controller.enqueue(encoder.encode(String(event.detail)))
          }
          remove = () => window.removeEventListener('observation-fixture', receive)
          window.addEventListener('observation-fixture', receive)
          init?.signal?.addEventListener(
            'abort',
            () => {
              remove()
              controller.close()
            },
            { once: true },
          )
          controller.enqueue(encoder.encode('event: live_snapshot\ndata: {"blocks":[]}\n\n'))
        },
        cancel() {
          remove()
        },
      })
      return new Response(body, { headers: { 'Content-Type': 'text/event-stream' } })
    }
  })
}

async function sendObservation(page: Page, name: string, data: unknown, sequence?: number): Promise<void> {
  const message = `${sequence === undefined ? '' : `id: ${sequence}\n`}event: ${name}\ndata: ${JSON.stringify(data)}\n\n`
  await page.evaluate((detail) => window.dispatchEvent(new CustomEvent('observation-fixture', { detail })), message)
}

function node(page: Page, name: string, status: string): Locator {
  return page.getByRole('button', { name: `${name}, ${statusLabels[status] ?? status}`, exact: true })
}

async function scrollListToBottom(target: Locator): Promise<void> {
  await target.evaluate((element) => {
    let candidate: HTMLElement | null = element.parentElement
    while (candidate) {
      const overflowY = getComputedStyle(candidate).overflowY
      if ((overflowY === 'auto' || overflowY === 'scroll') && candidate.scrollHeight > candidate.clientHeight) {
        candidate.scrollTop = candidate.scrollHeight
        return
      }
      candidate = candidate.parentElement
    }
  })
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

  test('a large chain mounts only its viewport instead of measuring every card at the origin', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    for (let index = 0; index < 300; index++) {
      fixture.addInteraction(
        interaction(
          `large-${index}`,
          'root-a',
          index ? `large-${index - 1}` : 'interaction-cinder',
          `Large ${index}`,
          routeIds.cinder,
          index === 299 ? 'running' : 'completed',
          150_000 + index,
        ),
      )
    }
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.addInitScript(() => {
      const probe = { peak: 0 }
      Object.assign(window, { canvasMountProbe: probe })
      new MutationObserver(() => {
        probe.peak = Math.max(probe.peak, document.querySelectorAll('.svelte-flow__node').length)
      }).observe(document, { childList: true, subtree: true })
    })
    await page.goto('/logs')
    await expect(node(page, 'Large 299', 'running')).toBeVisible()
    await expect(page.getByRole('link', { name: 'Svelte Flow' })).toHaveCount(0)
    const peak = await page.evaluate(
      () => (window as unknown as { canvasMountProbe: { peak: number } }).canvasMountProbe.peak,
    )
    expect(peak).toBeLessThan(80)
    await node(page, 'Large 299', 'running').click()
    await expect(
      page
        .getByRole('complementary', { name: 'Observation details' })
        .getByRole('heading', { name: 'Large 299', exact: true, level: 2 }),
    ).toBeVisible()
    await page.getByRole('button', { name: 'Close', exact: true }).click()
    for (let index = 300; index < 600; index++) {
      fixture.addInteraction(
        interaction(
          `large-${index}`,
          'root-a',
          `large-${index - 1}`,
          `Large ${index}`,
          routeIds.cinder,
          index === 599 ? 'running' : 'completed',
          150_000 + index,
        ),
      )
    }
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'large-599',
      run_id: 'run-large-599',
      rejection_id: null,
      kind: 'model_turn_started',
      payload: {},
    })
    await expect.poll(() => fixture.summaryRequests.some((url) => url.pathname.includes('large-599'))).toBe(true)
    await page.getByRole('button', { name: 'Return to running interaction', exact: true }).click()
    await expect(node(page, 'Large 599', 'running')).toBeVisible()
    await expect(node(page, 'Large 299', 'running')).toHaveCount(0)
    expect(
      await page.evaluate(() => (window as unknown as { canvasMountProbe: { peak: number } }).canvasMountProbe.peak),
    ).toBeLessThan(80)
  })

  test('live revisions render before saving, commit once, and clear on snapshot or disconnect', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await installPersistentObservationStream(page)
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/logs?interaction=interaction-cinder')
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    const conversation = inspector.getByRole('log', { name: 'Conversation' })
    await expect(conversation).toContainText('Cinder client-visible answer')
    await expect.poll(() => fixture.eventRequests.length).toBe(1)
    const reads = fixture.eventRequests.length
    const block = {
      block_id: 'live-table',
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      kind: 'client_visible_content_delta',
      model_turn_id: 'turn',
      attempt_id: 'attempt',
      occurred_at: startedAt + 299_000,
      revision: 1,
      text: '\n\n| A | B |\n',
    }
    await sendObservation(page, 'live_content', block)
    await expect(conversation.getByText('Not yet saved', { exact: true })).toBeVisible()
    const completed = { ...block, revision: 2, text: `${block.text}|---|---|\n| live-cell | other |` }
    await sendObservation(page, 'live_content', completed)
    await expect(conversation.getByRole('cell', { name: 'live-cell', exact: true })).toBeVisible()
    const renderedTable = await conversation.getByRole('table').elementHandle()
    await sendObservation(page, 'live_content', block)
    await expect(conversation.getByRole('cell', { name: 'live-cell', exact: true })).toHaveCount(1)
    expect(fixture.eventRequests).toHaveLength(reads)
    expect(fixture.detailRequests).toHaveLength(1)
    const durable: ObservationEvent = {
      sequence: 11,
      occurred_at: block.occurred_at,
      interaction_id: block.interaction_id,
      run_id: block.run_id,
      rejection_id: null,
      kind: block.kind,
      payload: { text: completed.text, block_id: block.block_id },
    }
    fixture.emit(durable)
    await sendObservation(page, 'observation', durable, durable.sequence)
    await expect(conversation.getByText('Not yet saved', { exact: true })).toHaveCount(0)
    await expect(conversation.getByRole('cell', { name: 'live-cell', exact: true })).toHaveCount(1)
    expect(await renderedTable!.evaluate((element) => element.isConnected)).toBe(true)
    expect(fixture.detailRequests).toHaveLength(1)
    await sendObservation(page, 'live_content', { ...block, block_id: 'transient', text: '\n\ntransient-unsaved' })
    await expect(conversation).toContainText('transient-unsaved')
    await sendObservation(page, 'live_snapshot', { blocks: [] })
    await expect(conversation).not.toContainText('transient-unsaved')
    await sendObservation(page, 'live_content', { ...block, block_id: 'failed', text: '\n\nfailed-unsaved' })
    await sendObservation(page, 'live_gap', {
      interaction_id: block.interaction_id,
      run_id: block.run_id,
      reason: 'live_capacity',
    })
    await expect(inspector.getByRole('alert')).toContainText('Live preview is incomplete')
    await expect(inspector).not.toContainText('could not be saved')
    await sendObservation(page, 'live_gap', {
      interaction_id: block.interaction_id,
      run_id: block.run_id,
      reason: 'persistence_failed',
    })
    await expect(inspector.getByRole('alert').filter({ hasText: 'could not be saved' })).toBeVisible()
    await page.evaluate(() => window.dispatchEvent(new CustomEvent('observation-fixture', { detail: 'disconnect' })))
    await expect(conversation).not.toContainText('failed-unsaved')
    await expect(conversation.getByRole('cell', { name: 'live-cell', exact: true })).toHaveCount(1)
  })

  test('incremental pages keep one upper bound and retry a failed read without dropping or duplicating text', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/logs?interaction=interaction-cinder')
    const conversation = page.getByRole('log', { name: 'Conversation' })
    await expect(conversation).toContainText('Cinder client-visible answer')
    await expect.poll(() => fixture.eventRequests.length).toBe(1)
    let failures = 0
    await page.route('**/observations/interactions/interaction-cinder/events?**', async (route) => {
      if (failures++ === 0) await route.fulfill({ status: 503, json: { error: { message: 'Temporary read failure' } } })
      else await route.fallback()
    })
    for (let index = 1; index <= 450; index++)
      fixture.emit({
        sequence: index + 10,
        occurred_at: startedAt + 299_000,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'client_visible_content_delta',
        payload: { text: `\n\nIncrement ${index}` },
      })
    await expect(conversation.getByText('Increment 450', { exact: true })).toHaveCount(1)
    await expect(conversation.getByText('Increment 1', { exact: true })).toHaveCount(1)
    await expect(conversation.getByText('Increment 201', { exact: true })).toHaveCount(1)
    const pages = fixture.eventRequests.slice(1)
    expect(pages.map((url) => url.searchParams.get('after_sequence'))).toEqual(['10', '210', '410'])
    expect(pages.map((url) => url.searchParams.get('through_sequence'))).toEqual([null, '460', '460'])
    expect(fixture.detailRequests).toHaveLength(1)
  })

  test('latest details load bounded history and prepend earlier events without moving the reading anchor', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page, false, false, false, (detail) => {
      if (detail.interaction.id !== 'interaction-cinder') return
      detail.runs[0].events = Array.from({ length: 450 }, (_, index) => ({
        sequence: index + 1,
        occurred_at: startedAt + index,
        interaction_id: detail.interaction.id,
        run_id: detail.runs[0].id,
        rejection_id: null,
        kind: 'client_visible_content_delta',
        payload: { text: `Paragraph ${index + 1}\n\n` },
      }))
    })
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/logs?interaction=interaction-cinder')
    const conversation = page.getByRole('log', { name: 'Conversation' })
    await expect(conversation).toContainText('Paragraph 251')
    await expect(conversation.getByText('Paragraph 250', { exact: true })).toHaveCount(0)
    const earlier = conversation.getByRole('button', { name: 'Load earlier events', exact: true })
    await expect(earlier).toHaveCount(0)
    const userPreview = conversation.getByText('Cinder user question', { exact: true })
    await conversation.evaluate((element) => {
      element.scrollTop = 0
      element.dispatchEvent(new Event('scroll'))
    })
    const userBefore = await userPreview.evaluate((element) => element.getBoundingClientRect().top)
    await conversation.getByText('Paragraph 51', { exact: true }).waitFor({ state: 'attached', timeout: 1000 }).catch(() => undefined)
    expect(
      Math.abs((await userPreview.evaluate((element) => element.getBoundingClientRect().top)) - userBefore),
    ).toBeLessThan(3)
    await expect(userPreview).toBeInViewport()
    const alignOlderSentinel = () =>
      conversation.evaluate((element) => {
        const sentinel = element.querySelector('[data-conversation-older-sentinel]')
        if (!(sentinel instanceof HTMLElement)) throw new Error('missing older-events sentinel')
        element.scrollTop += sentinel.getBoundingClientRect().top - element.getBoundingClientRect().top
        return [...element.querySelectorAll('p')].find((paragraph) => paragraph.textContent === 'Paragraph 251')!.getBoundingClientRect().top
      })
    const anchor = conversation.getByText('Paragraph 251', { exact: true })
    const before = await alignOlderSentinel()
    await expect(conversation.getByText('Paragraph 51', { exact: true })).toHaveCount(1)
    await expect
      .poll(async () => Math.abs((await anchor.evaluate((element) => element.getBoundingClientRect().top)) - before))
      .toBeLessThan(3)
    await alignOlderSentinel()
    await expect(conversation.getByText('Paragraph 1', { exact: true })).toHaveCount(1)
    await expect(earlier).toHaveCount(0)
    expect(fixture.detailRequests).toHaveLength(1)
    expect(
      fixture.eventRequests
        .filter((url) => url.searchParams.has('before_sequence'))
        .map((url) => url.searchParams.get('before_sequence')),
    ).toEqual(['251', '51'])
  })

  test('refreshes unopened previews and new interactions through summaries without fetching details', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' Summary-only live answer' },
    })
    await expect(node(page, 'Cinder', 'running').locator('article')).toContainText('Summary-only live answer')
    expect(fixture.summaryRequests.map((url) => url.pathname)).toEqual([
      '/api/v1/observations/interactions/interaction-cinder/summary',
    ])
    const query = fixture.summaryRequests[0].searchParams
    expect(Number(query.get('end_at')) - Number(query.get('start_at'))).toBe(600_000)
    expect(fixture.detailRequests).toEqual([])

    fixture.addInteraction(
      interaction('interaction-nova', 'root-a', 'interaction-cinder', 'Nova', routeIds.cinder, 'running', 298_000),
    )
    fixture.emit({
      sequence: 12,
      occurred_at: startedAt + 299_100,
      interaction_id: 'interaction-nova',
      run_id: 'run-interaction-nova',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' New child preview' },
    })
    await expect(node(page, 'Nova', 'running').locator('article')).toContainText('New child preview')
    await expect(node(page, 'Atlas', 'completed')).toHaveCount(1)
    expect(fixture.summaryRequests.at(-1)?.pathname).toBe('/api/v1/observations/interactions/interaction-nova/summary')
    expect(fixture.detailRequests).toEqual([])
    await expect(page.getByRole('complementary', { name: 'Observation details' })).toHaveCount(0)
  })

  test('updates the selected conversation through event pages without fetching detail again', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/logs?interaction=interaction-cinder')
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    const conversation = inspector.getByRole('log', { name: 'Conversation' })
    await expect(conversation).toContainText('Cinder client-visible answer')
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' Ordinary live continuation' },
    })
    await expect(conversation).toContainText('Ordinary live continuation')
    await expect(node(page, 'Cinder', 'running').locator('article')).toContainText('Ordinary live continuation')
    expect(fixture.detailRequests).toHaveLength(1)
    expect(fixture.summaryRequests).toHaveLength(1)
    expect(fixture.eventRequests.at(-1)?.searchParams.get('after_sequence')).toBe('10')

    fixture.emit({
      sequence: 12,
      occurred_at: startedAt + 299_100,
      interaction_id: 'interaction-boreal',
      run_id: 'run-interaction-boreal',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' Sibling summary update' },
    })
    await expect(node(page, 'Boreal', 'waiting_client').locator('article')).toContainText('Sibling summary update')
    await expect(conversation).toContainText('Ordinary live continuation')
    expect(fixture.detailRequests).toHaveLength(1)
    expect(fixture.summaryRequests).toHaveLength(2)
  })

  for (const pending of ['initial', 'live'] as const) {
    for (const action of ['switch', 'close'] as const) {
      test(`${action} prevents a pending ${pending} detail from restoring the previous selection`, async ({ page }) => {
        const fixture = await installObservationFixture(page)
        await page.goto('/logs')
        const inspector = page.getByRole('complementary', { name: 'Observation details' })
        let release: () => void
        if (pending === 'initial') {
          release = fixture.holdNextRead('interaction-cinder')
          await node(page, 'Cinder', 'running').getByRole('heading', { name: 'Cinder', exact: true }).click()
        } else {
          await node(page, 'Cinder', 'running').getByRole('heading', { name: 'Cinder', exact: true }).click()
          await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText(
            'Cinder client-visible answer',
          )
          await expect.poll(() => fixture.eventRequests.length).toBe(1)
          release = fixture.holdNextRead('interaction-cinder')
          fixture.emit({
            sequence: 11,
            occurred_at: startedAt + 299_000,
            interaction_id: 'interaction-cinder',
            run_id: 'run-interaction-cinder',
            rejection_id: null,
            kind: 'client_visible_content_delta',
            payload: { text: ' Delayed Cinder update' },
          })
        }
        await expect
          .poll(() => (pending === 'initial' ? fixture.detailRequests.length : fixture.eventRequests.length))
          .toBe(pending === 'initial' ? 1 : 2)
        if (action === 'switch') {
          await node(page, 'Atlas', 'completed').focus()
          await node(page, 'Atlas', 'completed').press('Enter')
          await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText(
            'Atlas client-visible answer',
          )
        } else await inspector.getByRole('button', { name: 'Close', exact: true }).click()
        const response = page.waitForResponse(
          (item) =>
            new URL(item.url()).pathname ===
            `/api/v1/observations/interactions/interaction-cinder${pending === 'live' ? '/events' : ''}`,
        )
        release()
        await (await response).finished()
        if (pending === 'live')
          await expect(node(page, 'Cinder', 'running').locator('article')).toContainText('Delayed Cinder update')
        if (action === 'switch') {
          await expect(inspector.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
          await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText(
            'Atlas client-visible answer',
          )
          await expect(inspector.getByRole('log', { name: 'Conversation' })).not.toContainText(
            'Cinder client-visible answer',
          )
        } else await expect(inspector).toHaveCount(0)
      })
    }
  }

  test('removes a filtered root only when its final matching interaction stops matching', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    await page.getByRole('button', { name: 'Filters' }).click()
    await page.getByLabel('Status', { exact: true }).click()
    await page.getByRole('option', { name: 'Running', exact: true }).click()
    await page.getByRole('button', { name: 'Apply filters' }).click()
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    await expect(node(page, 'Atlas', 'completed')).toBeVisible()
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'delivery_terminal',
      payload: { delivered: true },
    })
    await expect(page.getByText('No interaction chains match', { exact: true })).toBeVisible()
    expect(fixture.summaryRequests.at(-1)?.searchParams.get('status')).toBe('running')
    expect(fixture.detailRequests).toEqual([])
  })

  test('hides chains below the default token total and can show all', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' })
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    await expect.poll(() => fixture.forestRequests.at(-1)?.searchParams.get('min_tokens')).toBe('10000')
    await page.getByRole('button', { name: 'Filters' }).click()
    const slider = page.getByRole('slider', { name: 'Minimum chain tokens' })
    await expect(slider).toBeVisible()
    const track = page.locator('[data-slot="slider-track"]')
    await expect(track).toBeVisible()
    await expect(page.locator('[data-slot="slider-range"]')).toBeVisible()
    await expect(slider).toHaveAttribute('aria-valuenow', '4')
    await slider.press('ArrowRight')
    await expect(slider).toHaveAttribute('aria-valuenow', '5')
    await expect(page.getByText('20K', { exact: true })).toBeVisible()
    await page.emulateMedia({ colorScheme: 'light' })
    await expect(track).toBeVisible()
    await track.click({ position: { x: 1, y: 2 } })
    await expect(slider).toHaveAttribute('aria-valuenow', '0')
    await page.getByRole('button', { name: 'Clear filters' }).click()
    await expect(slider).toHaveAttribute('aria-valuenow', '0')
    await page.getByRole('button', { name: 'Apply filters' }).click()
    await expect.poll(() => fixture.forestRequests.at(-1)?.searchParams.has('min_tokens')).toBe(false)
  })

  test('keeps a historical root updated through summaries and exposes its migration when inspected', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    await page.getByRole('button', { name: 'Choose date and time range', exact: true }).click()
    await page
      .getByRole('dialog', { name: 'Date and time range', exact: true })
      .getByRole('button', { name: 'Apply', exact: true })
      .click()
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 301_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' Historical chain advanced' },
    })
    await expect(node(page, 'Cinder', 'running').locator('article')).toContainText('Historical chain advanced')
    expect(fixture.detailRequests).toEqual([])
    expect(Number(fixture.summaryRequests[0].searchParams.get('end_at'))).toBe(startedAt + 300_000)
    await node(page, 'Cinder', 'running').getByRole('heading', { name: 'Cinder', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(
      inspector.getByRole('button', { name: 'This chain moved to a newer time page · Open latest', exact: true }),
    ).toBeVisible()
    await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText('Historical chain advanced')
    expect(fixture.detailRequests).toHaveLength(1)
  })

  test('a pending direct link cannot override a later canvas selection', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    const release = fixture.holdNextRead('interaction-ember')
    await page.goto('/logs?interaction=interaction-ember')
    await expect.poll(() => fixture.detailRequests.length).toBe(1)
    await node(page, 'Atlas', 'completed').getByRole('heading', { name: 'Atlas', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText('Atlas client-visible answer')
    const response = page.waitForResponse(
      (item) => new URL(item.url()).pathname === '/api/v1/observations/interactions/interaction-ember',
    )
    release()
    await (await response).finished()
    await expect(inspector.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText('Atlas client-visible answer')
  })

  test('reset recovery refreshes an open conversation as well as the forest', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.emulateMedia({ reducedMotion: 'reduce' })
    await page.goto('/logs?interaction=interaction-cinder')
    const conversation = page
      .getByRole('complementary', { name: 'Observation details' })
      .getByRole('log', { name: 'Conversation' })
    await expect(conversation).toContainText('Cinder client-visible answer')
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' Recovered after replay gap' },
    })
    fixture.reset()
    await expect(conversation).toContainText('Recovered after replay gap')
    await expect(node(page, 'Cinder', 'running').locator('article')).toContainText('Recovered after replay gap')
    expect(fixture.detailRequests).toHaveLength(2)
    expect(fixture.summaryRequests).toEqual([])
  })

  test('a failed summary leaves the stream cursor replayable and recovers the preview without details', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    let attempts = 0
    await page.route('**/api/v1/observations/interactions/interaction-cinder/summary?**', async (route) => {
      attempts += 1
      if (attempts === 1) await route.fulfill({ status: 503, json: { error: 'Summary unavailable' } })
      else await route.fallback()
    })
    await page.goto('/logs')
    await expect(node(page, 'Cinder', 'running')).toBeVisible()
    fixture.emit({
      sequence: 11,
      occurred_at: startedAt + 299_000,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'client_visible_content_delta',
      payload: { text: ' Replayed summary answer' },
    })
    await expect(node(page, 'Cinder', 'running').locator('article')).toContainText('Replayed summary answer')
    expect(attempts).toBe(2)
    expect(fixture.detailRequests).toEqual([])
  })

  test('fills remaining window space in both tabs and keeps failed request rows and footer actions reachable', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    for (let index = 0; index < 31; index++) {
      fixture.addFailure(
        failedRequest(`list-${index}`, index % 2 === 0 ? 'run' : 'rejection', startedAt + 10_000 + index * 1_000, {
          api_key_name: 'Ops key',
          model_display_name: `Model ${index}`,
          services: [{ id: 'provider-fixture', name: 'Fixture Provider' }],
          error: {
            source: 'upstream',
            code: 'upstream_error',
            message: `Upstream request ${index} closed the connection before the response completed`,
            status_code: 502,
          },
          duration_ms: 1_500 + index,
        }),
      )
    }
    await page.goto('/logs')
    await expect(node(page, 'Atlas', 'completed')).toBeVisible()

    for (const viewport of [
      { width: 1440, height: 1200 },
      { width: 1280, height: 640 },
      { width: 500, height: 800 },
      { width: 320, height: 740 },
    ]) {
      await page.setViewportSize(viewport)
      for (const tab of ['Interaction Chains', 'Failed Requests']) {
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
      const table = page.getByRole('table', { name: 'Failed Requests', exact: true })
      await expect(table).toBeVisible()
      const lastRowTime = table.getByRole('button').last()
      await scrollListToBottom(lastRowTime)
      await expect(lastRowTime).toBeInViewport()
      await expect(page.getByRole('button', { name: 'Load more', exact: true })).toBeInViewport()
    }
  })

  test('renders the seven failure columns with resolved and unknown values without widening the page', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    const longMessage = `Upstream stream terminated after HTTP 200: ${'unexpected connection reset while streaming '.repeat(30)}end of transcript`
    fixture.addFailure(
      failedRequest('run-platform', 'run', startedAt + 250_000, {
        duration_ms: 2_500,
        api_key_id: 'key-ops',
        api_key_name: 'Ops key',
        model: 'ember-pro',
        model_display_name: 'Ember Pro',
        services: [
          { id: 'provider-ember', name: 'Ember Provider' },
          { id: 'provider-fallback', name: 'Fallback Provider' },
        ],
        error: {
          source: 'platform',
          code: 'run_failed',
          message: 'Model turn ended in failure before any output was committed',
          status_code: null,
        },
        interaction_id: 'interaction-delta',
        root_id: 'root-b',
        run_id: 'run-interaction-delta',
      }),
    )
    fixture.addFailure(
      failedRequest('rejection-decode', 'rejection', startedAt + 200_000, {
        error: { source: null, code: 'invalid_request', message: null, status_code: 400 },
      }),
    )
    fixture.addFailure(
      failedRequest('run-upstream', 'run', startedAt + 150_000, {
        duration_ms: 12_000,
        client: 'codex',
        model: 'zhipu/glm-4.7',
        error: { source: 'upstream', code: 'upstream_error', message: longMessage, status_code: 200 },
      }),
    )
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const table = page.getByRole('table', { name: 'Failed Requests', exact: true })
    await expect(table.getByRole('columnheader')).toHaveText([
      'Time',
      'Client / API Key',
      'Model',
      'Model service',
      'Error source',
      'Error',
      'Duration',
    ])

    const rowByTime = (timestamp: number) =>
      table.getByRole('button', { name: logTime(timestamp), exact: true }).locator('xpath=ancestor::tr[1]')
    const platformRow = rowByTime(startedAt + 250_000)
    await expect(platformRow).toContainText('Ops key')
    await expect(platformRow).toContainText('Ember Pro')
    await expect(platformRow).toContainText('Ember Provider, Fallback Provider')
    await expect(platformRow).toContainText('Platform')
    await expect(platformRow).toContainText('Model turn ended in failure before any output was committed')
    await expect(platformRow).toContainText('2.5 s')
    await expect(platformRow).not.toContainText('HTTP')

    const rejectionRow = rowByTime(startedAt + 200_000)
    await expect(rejectionRow).toContainText('Unauthenticated')
    await expect(rejectionRow).toContainText('invalid_request')
    await expect(rejectionRow).toContainText('HTTP 400')
    await expect(rejectionRow.getByText('—', { exact: true })).toHaveCount(4)

    const upstreamRow = rowByTime(startedAt + 150_000)
    await expect(upstreamRow).toContainText('codex')
    await expect(upstreamRow).toContainText('zhipu/glm-4.7')
    await expect(upstreamRow).toContainText('Upstream')
    await expect(upstreamRow).toContainText('HTTP 200')
    await expect(upstreamRow).toContainText('12 s')

    await expect(table.getByRole('button').first()).toHaveText(logTime(startedAt + 250_000))
    await expect(table.getByRole('button').last()).toHaveText(logTime(startedAt + 150_000))
    const boundedRow = await rejectionRow.boundingBox()
    const longRow = await upstreamRow.boundingBox()
    expect(Math.abs(longRow!.height - boundedRow!.height)).toBeLessThanOrEqual(1)

    await page.setViewportSize({ width: 320, height: 740 })
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(320)
    await table.getByRole('button').last().click()
    const dialog = page.getByRole('dialog', { name: 'Observation details' })
    await expect(dialog.getByRole('heading', { name: 'Request failed', exact: true, level: 2 })).toBeVisible()
    await expect(dialog.getByText(longMessage, { exact: true })).toBeVisible()
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(320)
    await dialog.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(dialog).toHaveCount(0)
  })

  test('opens an unassociated failure detail with the complete error and no fabricated jump', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    const longError = `Decoding stopped at byte 512: ${'malformed UTF-8 continuation sequence '.repeat(28)}buffer exhausted`
    fixture.addFailure(
      failedRequest('rejection-decode', 'rejection', startedAt + 220_000, {
        error: { source: null, code: 'invalid_request', message: longError, status_code: 400 },
      }),
      {
        events: [
          {
            sequence: 21,
            occurred_at: startedAt + 220_004,
            interaction_id: null,
            run_id: null,
            rejection_id: 'rejection-decode',
            kind: 'request_rejected',
            payload: { stage: 'decode', status_code: 400 },
          },
        ],
      },
    )
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const rowTime = page
      .getByRole('table', { name: 'Failed Requests', exact: true })
      .getByRole('button', { name: logTime(startedAt + 220_000), exact: true })
    await rowTime.click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector.getByText('Failed request details', { exact: true })).toBeVisible()
    await expect(inspector.getByRole('heading', { name: 'Request failed', exact: true, level: 2 })).toBeVisible()
    await expect(inspector.getByText(longError, { exact: true })).toBeVisible()
    await expect(inspector.getByText('req-rejection-decode', { exact: true })).toBeVisible()
    await expect(inspector.getByText('HTTP 400', { exact: true })).toBeVisible()
    await expect(inspector.getByText('Not captured', { exact: true })).toBeVisible()
    await expect(inspector.getByText('Request rejected', { exact: true })).toBeVisible()
    await expect(inspector.getByRole('button', { name: 'Open interaction node', exact: true })).toHaveCount(0)
    await expect(inspector.getByRole('button', { name: 'Debug bundle', exact: true })).toBeVisible()
    await inspector.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(inspector).toHaveCount(0)

    let detailAttempts = 0
    const failDetail = async (route: Route) => {
      detailAttempts += 1
      if (detailAttempts === 1) await route.fulfill({ status: 503, json: { error: 'Failure detail unavailable' } })
      else await route.fallback()
    }
    await page.route('**/api/v1/observations/failed-requests/rejection/**', failDetail)
    await rowTime.click()
    const detailError = page.getByRole('alert').filter({ hasText: 'Failure detail unavailable' })
    await expect(detailError).toBeVisible()
    await expect(inspector.getByText(longError, { exact: true })).toHaveCount(0)
    await detailError.getByRole('button', { name: 'Retry', exact: true }).click()
    await expect(inspector.getByText(longError, { exact: true })).toBeVisible()
    expect(detailAttempts).toBe(2)
    await page.unroute('**/api/v1/observations/failed-requests/rejection/**', failDetail)
  })

  test('returns from a failure to its linked interaction node even after the interaction completed', async ({
    page,
  }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' })
    const fixture = await installObservationFixture(page)
    fixture.addFailure(
      failedRequest('run-ember', 'run', startedAt + 245_000, {
        duration_ms: 4_800,
        api_key_name: 'Ops key',
        client: 'codex',
        model_display_name: 'Ember',
        services: [{ id: 'provider-ember', name: 'Ember Provider' }],
        error: {
          source: 'upstream',
          code: 'upstream_timeout',
          message: 'Upstream request timed out after 4800 ms',
          status_code: 504,
        },
        interaction_id: 'interaction-ember',
        root_id: 'root-c',
        run_id: 'run-interaction-ember',
        debug_status: 'partial',
      }),
      {
        events: [
          {
            sequence: 31,
            occurred_at: startedAt + 240_200,
            interaction_id: 'interaction-ember',
            run_id: 'run-interaction-ember',
            rejection_id: null,
            kind: 'run_finished',
            payload: { status: 'failed', reason: 'upstream_timeout' },
          },
        ],
        trace: {
          trace_id: 'trace-ember',
          enabled: true,
          status: 'partial',
          bytes_written: 4_096,
          event_count: 12,
          reasons: ['Trace stopped when the process restarted.'],
        },
      },
    )
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    await page
      .getByRole('table', { name: 'Failed Requests', exact: true })
      .getByRole('button', { name: logTime(startedAt + 245_000), exact: true })
      .click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await expect(inspector.getByText('Upstream request timed out after 4800 ms', { exact: true })).toBeVisible()
    await expect(inspector.getByText('Partial', { exact: true })).toBeVisible()
    await expect(inspector.getByText('Trace stopped when the process restarted.', { exact: true })).toBeVisible()
    await inspector.getByRole('button', { name: 'Open interaction node', exact: true }).click()
    await expect(page.getByRole('tab', { name: 'Interaction Chains', exact: true })).toHaveAttribute(
      'aria-selected',
      'true',
    )
    await expect(inspector).toHaveCount(0)
    const emberNode = node(page, 'Ember', 'completed')
    await expect(emberNode).toBeVisible()
    await expect(emberNode).toBeInViewport()
    await expect
      .poll(async () => {
        const target = await emberNode.boundingBox()
        const viewport = await page.locator('.svelte-flow__pane').boundingBox()
        if (!target || !viewport) return Number.POSITIVE_INFINITY
        return Math.hypot(
          target.x + target.width / 2 - viewport.x - viewport.width / 2,
          target.y + target.height / 2 - viewport.y - viewport.height / 2,
        )
      })
      .toBeLessThan(32)
    await emberNode.click()
    await expect(inspector.getByRole('heading', { name: 'Ember', exact: true, level: 2 })).toBeVisible()
    await expect(inspector.getByRole('log', { name: 'Conversation' })).toContainText('Ember client-visible answer')
  })

  test('pages and refreshes failed requests without duplicating equal-timestamp rows', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    for (let index = 0; index < 29; index++) {
      fixture.addFailure(
        failedRequest(`page-${index}`, 'run', startedAt + 60_000 + index * 1_000, {
          api_key_name: 'Ops key',
          error: {
            source: 'platform',
            code: 'run_failed',
            message: `Platform request ${index} failed`,
            status_code: null,
          },
          duration_ms: 3_000 + index,
        }),
      )
    }
    fixture.addFailure(
      failedRequest('boundary-rejection', 'rejection', startedAt + 50_000, {
        error: {
          source: null,
          code: 'invalid_request',
          message: 'Boundary rejection failed before admission',
          status_code: 400,
        },
      }),
    )
    fixture.addFailure(
      failedRequest('boundary-run', 'run', startedAt + 50_000, {
        client: 'codex',
        error: {
          source: 'upstream',
          code: 'upstream_error',
          message: 'Boundary run failed after admission',
          status_code: 500,
        },
        duration_ms: 9_000,
      }),
    )
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const table = page.getByRole('table', { name: 'Failed Requests', exact: true })
    await expect(table.getByRole('button')).toHaveCount(30)
    await expect(page.getByText('30 / 31', { exact: true })).toBeVisible()
    await expect(table.getByRole('button', { name: logTime(startedAt + 50_000), exact: true })).toHaveCount(1)

    await page.getByRole('button', { name: 'Load more', exact: true }).click()
    await expect(table.getByRole('button')).toHaveCount(31)
    await expect(page.getByText('31 / 31', { exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Load more', exact: true })).toHaveCount(0)
    const boundaryRows = table
      .getByRole('row')
      .filter({ has: page.getByRole('button', { name: logTime(startedAt + 50_000), exact: true }) })
    await expect(boundaryRows).toHaveCount(2)
    await expect(boundaryRows.filter({ hasText: 'Unauthenticated' })).toHaveCount(1)
    await expect(boundaryRows.filter({ hasText: 'codex' })).toHaveCount(1)

    await page.getByRole('button', { name: 'Refresh and anchor a new current window' }).click()
    await expect(table.getByRole('button')).toHaveCount(30)
    await expect(page.getByText('30 / 31', { exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: 'Load more', exact: true })).toBeInViewport()
  })

  test('separates the empty, failed, and loading states of the failed request list', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    await expect(page.getByText('No failed requests', { exact: true })).toBeVisible()
    await expect(
      page.getByText('No final request failures match this time range and these filters.', { exact: true }),
    ).toBeVisible()
    await expect(page.getByRole('table', { name: 'Failed Requests', exact: true })).toHaveCount(0)

    fixture.addFailure(
      failedRequest('state-run', 'run', startedAt + 90_000, {
        client: 'codex',
        error: {
          source: 'upstream',
          code: 'upstream_error',
          message: 'Only failure while checking states',
          status_code: 500,
        },
        duration_ms: 2_000,
      }),
    )
    let listAttempts = 0
    const failList = async (route: Route) => {
      listAttempts += 1
      if (listAttempts === 1) await route.fulfill({ status: 503, json: { error: 'Failed requests unavailable' } })
      else await route.fallback()
    }
    await page.route('**/api/v1/observations/failed-requests?**', failList)
    await page.getByRole('button', { name: 'Refresh and anchor a new current window' }).click()
    const listError = page.getByRole('alert').filter({ hasText: 'Failed requests unavailable' })
    await expect(listError).toBeVisible()
    await expect(page.getByText('No failed requests', { exact: true })).toHaveCount(0)
    await listError.getByRole('button', { name: 'Retry', exact: true }).click()
    const table = page.getByRole('table', { name: 'Failed Requests', exact: true })
    await expect(table.getByRole('button')).toHaveCount(1)
    await expect(listError).toHaveCount(0)
    await page.unroute('**/api/v1/observations/failed-requests?**', failList)

    const release = fixture.holdNextFailureRead()
    await page.getByRole('button', { name: 'Refresh and anchor a new current window' }).click()
    await expect(page.getByText('Loading failed requests…', { exact: true })).toBeVisible()
    await expect(page.getByText('No failed requests', { exact: true })).toHaveCount(0)
    await expect(table.getByRole('button')).toHaveCount(0)
    release()
    await expect(table.getByRole('button')).toHaveCount(1)
  })

  test('a failure between clock ticks becomes visible live without moving an applied fixed range', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    fixture.addFailure(
      failedRequest('run-steady', 'run', startedAt + 150_000, {
        client: 'codex',
        error: {
          source: 'upstream',
          code: 'upstream_error',
          message: 'Steady failure already inside the window',
          status_code: 502,
        },
        duration_ms: 4_000,
      }),
    )
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const table = page.getByRole('table', { name: 'Failed Requests', exact: true })
    const rowByTime = (timestamp: number) =>
      table.getByRole('button', { name: logTime(timestamp), exact: true }).locator('xpath=ancestor::tr[1]')
    await expect(table.getByRole('button', { name: logTime(startedAt + 150_000), exact: true })).toBeVisible()

    // The browser clock advances into the gap between two window advances; the rejection starts
    // and ends between two clock ticks, still at or before the browser's current time.
    await page.clock.setFixedTime(startedAt + 301_500)
    const rejectedAt = startedAt + 300_400
    fixture.addFailure(
      failedRequest('rejection-fresh', 'rejection', rejectedAt, {
        error: {
          source: null,
          code: 'rate_limited',
          message: 'Fresh rejection between two clock ticks',
          status_code: 429,
        },
      }),
    )
    fixture.emit({
      sequence: 11,
      occurred_at: rejectedAt,
      interaction_id: null,
      run_id: null,
      rejection_id: 'rejection-fresh',
      kind: 'request_rejected',
      payload: { stage: 'admission', status_code: 429 },
    })
    await expect(table.getByRole('button', { name: logTime(rejectedAt), exact: true })).toBeVisible()
    await expect(rowByTime(rejectedAt)).toContainText('Fresh rejection between two clock ticks')
    await expect(page.getByText('2 / 2', { exact: true })).toBeVisible()

    // An applied fixed range keeps its exact bounds and never absorbs newer failures.
    await page.getByRole('button', { name: 'Choose date and time range', exact: true }).click()
    const dialog = page.getByRole('dialog', { name: 'Date and time range', exact: true })
    await dialog.getByLabel('Start time', { exact: true }).fill(localRangeValue(60_000))
    await dialog.getByLabel('End time', { exact: true }).fill(localRangeValue(180_000))
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click()
    await expect(dialog).toBeHidden()
    await expect
      .poll(() => {
        const params = fixture.failureRequests.at(-1)?.searchParams
        return [Number(params?.get('start_at')), Number(params?.get('end_at'))]
      })
      .toEqual([startedAt + 60_000, startedAt + 180_000])
    await expect(table.getByRole('button', { name: logTime(startedAt + 150_000), exact: true })).toBeVisible()
    await expect(table.getByRole('button', { name: logTime(rejectedAt), exact: true })).toHaveCount(0)

    const failedAt = startedAt + 301_000
    fixture.addFailure(
      failedRequest('run-fresh', 'run', failedAt, {
        error: {
          source: 'platform',
          code: 'run_failed',
          message: 'Later failure must not move the fixed range',
          status_code: null,
        },
      }),
    )
    fixture.emit({
      sequence: 12,
      occurred_at: failedAt,
      interaction_id: 'interaction-cinder',
      run_id: 'run-interaction-cinder',
      rejection_id: null,
      kind: 'run_finished',
      payload: { status: 'failed', reason: 'upstream_timeout' },
    })
    await expect
      .poll(() => fixture.summaryRequests.some((url) => url.pathname.includes('interaction-cinder')))
      .toBe(true)
    await expect(table.getByRole('button', { name: logTime(failedAt), exact: true })).toHaveCount(0)
    await expect(table.getByRole('button', { name: logTime(rejectedAt), exact: true })).toHaveCount(0)
  })

  test('switching tabs or filters reloads failed requests from the advanced clock without moving a fixed range', async ({
    page,
  }) => {
    const fixture = await installObservationFixture(page)
    fixture.addFailure(
      failedRequest('run-baseline', 'run', startedAt + 150_000, {
        client: 'codex',
        error: {
          source: 'upstream',
          code: 'upstream_error',
          message: 'Baseline failure inside the loaded window',
          status_code: 502,
        },
        duration_ms: 3_000,
      }),
    )
    // Pausing before the first load freezes the one-second interval: the live window cannot advance itself.
    await page.clock.pauseAt(startedAt + 300_000)
    await page.goto('/logs')
    await expect.poll(() => fixture.forestRequests.length).toBeGreaterThanOrEqual(1)

    // The browser clock moves between two ticks; a rejection finishes before the browser's now, without any stream event.
    await page.clock.setSystemTime(startedAt + 301_000)
    const rejectedAt = startedAt + 300_400
    fixture.addFailure(
      failedRequest('rejection-tab', 'rejection', rejectedAt, {
        error: {
          source: null,
          code: 'rate_limited',
          message: 'Rejection that finished between two clock ticks',
          status_code: 429,
        },
      }),
    )
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const table = page.getByRole('table', { name: 'Failed Requests', exact: true })
    await expect(table.getByRole('button', { name: logTime(rejectedAt), exact: true })).toBeVisible()
    await expect(page.getByText('2 / 2', { exact: true })).toBeVisible()
    expect(Number(fixture.failureRequests[0].searchParams.get('end_at'))).toBeGreaterThan(rejectedAt)

    // Re-applying filters re-queries from the same advanced clock, not the stale window end.
    await page.clock.setSystemTime(startedAt + 302_000)
    const filteredAt = startedAt + 301_600
    fixture.addFailure(
      failedRequest('run-filter', 'run', filteredAt, {
        error: {
          source: 'platform',
          code: 'run_failed',
          message: 'Failure that finished after the tab switch',
          status_code: null,
        },
      }),
    )
    await page.getByRole('button', { name: 'Filters' }).click()
    await page.getByRole('button', { name: 'Apply filters' }).click()
    await expect(table.getByRole('button', { name: logTime(filteredAt), exact: true })).toBeVisible()
    await expect(page.getByText('3 / 3', { exact: true })).toBeVisible()
    // Freshness is proven with timers paused; let the filter sheet finish closing.
    await page.clock.resume()

    // An applied fixed range keeps its exact bounds and never absorbs newer failures.
    await page.getByRole('button', { name: 'Choose date and time range', exact: true }).click()
    const dialog = page.getByRole('dialog', { name: 'Date and time range', exact: true })
    await dialog.getByLabel('Start time', { exact: true }).fill(localRangeValue(60_000))
    await dialog.getByLabel('End time', { exact: true }).fill(localRangeValue(180_000))
    await dialog.getByRole('button', { name: 'Apply', exact: true }).click()
    await expect(dialog).toBeHidden()
    await expect
      .poll(() => {
        const params = fixture.failureRequests.at(-1)?.searchParams
        return [Number(params?.get('start_at')), Number(params?.get('end_at'))]
      })
      .toEqual([startedAt + 60_000, startedAt + 180_000])
    await expect(table.getByRole('button', { name: logTime(startedAt + 150_000), exact: true })).toBeVisible()
    await expect(table.getByRole('button', { name: logTime(rejectedAt), exact: true })).toHaveCount(0)
    await expect(table.getByRole('button', { name: logTime(filteredAt), exact: true })).toHaveCount(0)
  })

  test('opens and closes failure details by keyboard and inside fullscreen', async ({ page }) => {
    const fixture = await installObservationFixture(page)
    fixture.addFailure(
      failedRequest('run-keyboard', 'run', startedAt + 130_000, {
        client: 'codex',
        error: { source: 'upstream', code: 'upstream_error', message: 'Keyboard navigation failure', status_code: 502 },
        duration_ms: 2_000,
      }),
    )
    await page.goto('/logs')
    await page.getByRole('tab', { name: 'Failed Requests', exact: true }).click()
    const rowTime = page
      .getByRole('table', { name: 'Failed Requests', exact: true })
      .getByRole('button', { name: logTime(startedAt + 130_000), exact: true })
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    await rowTime.focus()
    await page.keyboard.press('Enter')
    await expect(inspector.getByRole('heading', { name: 'Request failed', exact: true, level: 2 })).toBeVisible()
    await expect(inspector.getByText('Keyboard navigation failure', { exact: true })).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(inspector).toHaveCount(0)
    await expect(rowTime).toBeFocused()

    await page.getByRole('button', { name: 'Enter fullscreen', exact: true }).click()
    await expect(page.getByRole('button', { name: 'Exit fullscreen', exact: true })).toBeVisible()
    await rowTime.click()
    await expect(inspector.getByRole('heading', { name: 'Request failed', exact: true, level: 2 })).toBeVisible()
    await inspector.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(inspector).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'Exit fullscreen', exact: true })).toBeVisible()
    await page.keyboard.press('Escape')
    await expect(page.getByRole('button', { name: 'Enter fullscreen', exact: true })).toBeVisible()
  })

  test('keeps an interjected user input on the user side before the next model response', async ({ page }) => {
    const input = '<system-notice>\nUser interjection during work: priority; supersedes conflicting prior instructions. Re-read; ensure current work reflects user intent.\n</system-notice>\n所有平台id 统一更换，包括 artifact id，所有 agent 可见id 以及外壳一起改'
    await installObservationFixture(page, false, false, false, (detail) => {
      if (detail.interaction.id !== 'interaction-atlas') return
      const first = detail.runs[0]
      detail.runs.push({
        ...first,
        id: 'run-interjection',
        parent_run_id: first.id,
        started_at: first.started_at + 1000,
        events: [
          { sequence: 20, occurred_at: first.started_at + 1000, interaction_id: detail.interaction.id, run_id: 'run-interjection', rejection_id: null, kind: 'input_preview_recorded', payload: { text: input } },
          { sequence: 21, occurred_at: first.started_at + 1001, interaction_id: detail.interaction.id, run_id: 'run-interjection', rejection_id: null, kind: 'client_visible_content_delta', payload: { text: '范围扩大为所有平台生成的 ID。' } },
        ],
      })
    })
    await page.goto('/logs?interaction=interaction-atlas')
    const conversation = page.getByRole('log', { name: 'Conversation' })
    const users = conversation.getByRole('article', { name: 'You', exact: true })
    await expect(users).toHaveCount(2)
    await expect(users.last()).toContainText('所有平台id 统一更换，包括 artifact id，所有 agent 可见id 以及外壳一起改')
    await expect(conversation.getByRole('article', { name: 'Atlas', exact: true }).last()).toContainText('范围扩大为所有平台生成的 ID。')
    expect(await conversation.getByRole('article').evaluateAll((articles) => articles.map((article) => article.getAttribute('aria-label')))).toEqual(['You', 'Atlas', 'You', 'Atlas'])
    await page.setViewportSize({ width: 390, height: 740 })
    await page.emulateMedia({ colorScheme: 'dark' })
    await users.last().scrollIntoViewIfNeeded()
    await expect(users.last()).toBeVisible()
    await expect(users.last()).toContainText('所有平台id 统一更换，包括 artifact id，所有 agent 可见id 以及外壳一起改')
  })

  test('opens readable conversation bubbles while retaining raw events in diagnostics', async ({ page }) => {
    await installObservationFixture(page, false, true)
    await page.goto('/logs')
    const atlas = node(page, 'Atlas', 'completed')
    const cardUsage = atlas.getByLabel('Confirmed usage')
    await expect(cardUsage.getByTitle('IN')).toContainText('920')
    await expect(cardUsage.getByTitle('OUT')).toContainText('86')
    await expect(cardUsage.getByTitle('C·R')).toContainText('320')
    await expect(cardUsage.getByTitle('C·W')).toContainText('–')
    await expect(cardUsage).not.toContainText('RSN')

    await atlas.getByRole('heading', { name: 'Atlas', exact: true }).click()
    const inspector = page.getByRole('complementary', { name: 'Observation details' })
    const conversation = inspector.getByRole('log', { name: 'Conversation' })
    await expect(conversation.getByRole('article', { name: 'You', exact: true })).toContainText('Atlas user question')
    await expect(conversation.getByRole('article', { name: 'Atlas', exact: true })).toContainText(
      'Atlas client-visible answer',
    )
    await expect(inspector.getByText('run-interaction-atlas', { exact: true })).toBeHidden()
    await expect(inspector.getByText('retained diagnostic record', { exact: false })).toBeHidden()
    await inspector.getByRole('tab', { name: 'Diagnostics', exact: true }).click()
    const runUsage = inspector.getByLabel('Confirmed usage')
    await expect(runUsage).toContainText('IN920')
    await expect(runUsage).toContainText('OUT86')
    await expect(runUsage).toContainText('C·R320')
    await expect(runUsage).toContainText('C·WNot reported')
    await expect(runUsage).not.toContainText('RSN')
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
    await expect(inspector.getByRole('tab')).toHaveText(['Conversation', 'Diagnostics'])
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
    test(`expands ordinary thinking and tool details with Debug ${captureDebug ? 'on' : 'off'} without storing content`, async ({
      page,
    }) => {
      let completed = false
      const fixture = await installObservationFixture(page, false, captureDebug, false, (detail) => {
        if (detail.interaction.id !== 'interaction-cinder') return
        const parent = detail.runs[0]
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
            payload: { tool_id: 'call-bash', name: 'Bash', input: { command: 'printf "tool-output-sentinel"' } },
          },
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
            ordinary(13, 'model_thinking_finished', { model_turn_id: 'activity-turn', attempt_id: 'activity-attempt' }),
            ordinary(14, 'platform_tool_started', {
              model_turn_id: 'activity-turn',
              tool_id: 'call-search',
              name: 'Web search',
              input: { query: 'fixture' },
            }),
            ordinary(15, 'platform_tool_finished', {
              model_turn_id: 'activity-turn',
              tool_id: 'call-search',
              status: 'completed',
              content: { answer: 'platform-output-sentinel' },
            }),
          )
        if (!completed) {
          parent.status = 'running'
          parent.finished_at = null
          parent.events = parent.events.filter((event) => event.kind !== 'client_tool_handoff')
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
          events: [
            ordinary(
              16,
              'client_tool_result',
              { tool_id: 'call-bash', content: 'tool-output-sentinel\n', is_error: false },
              childId,
            ),
          ],
        })
      })
      await page.goto('/logs?interaction=interaction-cinder')
      await expect(page.getByRole('tab', { name: 'Debug records', exact: true })).toHaveCount(0)
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
        kind: 'model_thinking_finished',
        payload: { model_turn_id: 'activity-turn', attempt_id: 'activity-attempt' },
      })
      await expect(thinking).toHaveAccessibleName('Thinking')
      await expect(thinking).toHaveAttribute('aria-expanded', 'true')
      await expect(tool).toHaveAttribute('aria-expanded', 'false')
      await expect(conversation.getByRole('article', { name: 'Cinder', exact: true })).toHaveCount(1)
      const responseTime = conversation.getByRole('article', { name: 'Cinder', exact: true }).locator('time')
      await expect(responseTime).toHaveCount(1)
      await expect(responseTime).toHaveAttribute('datetime', new Date(startedAt + 299_006).toISOString())
      await tool.press('Enter')
      await expect(conversation.getByRole('heading', { name: 'Tool input', exact: true })).toBeVisible()
      await expect(conversation.getByText('Client result', { exact: true })).toBeVisible()
      await expect(conversation.getByText(/printf/)).toBeVisible()
      await expect(conversation.getByText('tool-output-sentinel', { exact: true })).toBeVisible()
      const platform = conversation.getByRole('button', { name: 'Tool call Web search', exact: true })
      await expect(platform).toHaveAttribute('aria-expanded', 'false')
      await platform.press('Enter')
      await expect(conversation.getByText('Tool result', { exact: true })).toBeVisible()
      await expect(conversation.getByText(/platform-output-sentinel/)).toBeVisible()
      await thinking.click()
      await expect(thinking).toHaveAttribute('aria-expanded', 'false')
      await page.reload()
      await expect(thinking).toHaveAttribute('aria-expanded', 'false')
      await expect(tool).toHaveAttribute('aria-expanded', 'true')
      await expect(conversation.getByText('Client result', { exact: true })).toBeVisible()
      expect(await page.evaluate(() => JSON.stringify({ ...localStorage }))).not.toContain('tool-output-sentinel')
    })
  }

  test('renders multiple thinking Markdown paragraphs live and after reload', async ({ page }) => {
    const fixture = await installObservationFixture(page, false, false, true)
    await page.goto('/logs?interaction=interaction-cinder')
    const conversation = page.getByRole('log', { name: 'Conversation' })
    const thinking = conversation.getByRole('button', { name: /^Thinking(?:…)?$/ })
    const scope = { model_turn_id: 'paragraph-turn', attempt_id: 'paragraph-attempt' }
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
    emit(11, 'model_thinking_delta', { ...scope, text: '**Selecting top ' })
    await thinking.click()
    emit(12, 'model_thinking_delta', { ...scope, text: 'five candidate features**' })
    emit(13, 'model_thinking_delta', { ...scope, text: '\n\n**Implementing temp path and timestamp retrieval**' })
    const content = conversation.locator('.markdown-content').filter({ hasText: 'Selecting top' })
    const assertParagraphs = async () => {
      await expect(content.locator('p')).toHaveCount(2)
      await expect(content.locator('strong')).toHaveText([
        'Selecting top five candidate features',
        'Implementing temp path and timestamp retrieval',
      ])
      await expect(content).not.toContainText('****')
    }
    await assertParagraphs()
    emit(14, 'model_thinking_finished', scope)
    await expect(thinking).toHaveAccessibleName('Thinking')
    await page.reload()
    await thinking.click()
    await assertParagraphs()
  })

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
    const ticketRequest = page.waitForRequest((request) =>
      request.url().endsWith('/interaction-atlas/debug-bundle-tickets'),
    )
    await page.getByRole('button', { name: 'Debug bundle', exact: true }).click()
    expect((await ticketRequest).postDataJSON()).toEqual({})
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
      // Newest roots occupy left columns; Delta started after Atlas's tree.
      expect(deltaBox!.right).toBeLessThan(Math.min(borealBox!.x, cinderBox!.x))
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
    fixture.emit(
      {
        sequence: 12,
        occurred_at: startedAt + 299_100,
        interaction_id: 'interaction-cinder',
        run_id: 'run-interaction-cinder',
        rejection_id: null,
        kind: 'delivery_terminal',
        payload: { delivered: true },
      },
      'Completed through a filtered summary',
    )
    await expect(node(page, 'Cinder', 'completed').locator('article')).toContainText(
      'Completed through a filtered summary',
    )
    await expect(node(page, 'Cinder', 'completed').locator('article')).toHaveCSS('opacity', '1')
    await expect(node(page, 'Boreal', 'waiting_client')).toBeVisible()
    expect(fixture.summaryRequests.at(-1)?.searchParams.get('status')).toBe('completed')
    expect(fixture.detailRequests).toHaveLength(1)

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
    await expect
      .poll(() => {
        const params = fixture.forestRequests.at(-1)?.searchParams
        return Number(params?.get('end_at')) - Number(params?.get('start_at'))
      })
      .toBe(600_000)
    await expect(page.getByRole('button', { name: 'Time window', exact: true })).toHaveText('10 minutes')
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
    await desktop.getByRole('tab', { name: 'Diagnostics' }).click()
    await expect(desktop.getByRole('tabpanel', { name: 'Diagnostics', exact: true })).toBeVisible()

    await page.setViewportSize({ width: 390, height: 740 })
    const mobile = page.getByRole('dialog', { name: 'Observation details' })
    await expect(mobile.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await expect(mobile.getByRole('tab', { name: 'Diagnostics' })).toHaveAttribute('aria-selected', 'true')
    await expect(mobile.getByRole('tabpanel', { name: 'Diagnostics', exact: true })).toBeVisible()
    await expect(desktop).toBeHidden()

    await page.setViewportSize({ width: 1280, height: 800 })
    await expect(desktop.getByRole('heading', { name: 'Atlas', exact: true, level: 2 })).toBeVisible()
    await expect(desktop.getByRole('tab', { name: 'Diagnostics' })).toHaveAttribute('aria-selected', 'true')
    await expect(desktop.getByRole('tabpanel', { name: 'Diagnostics', exact: true })).toBeVisible()
    await expect(mobile).toBeHidden()
    await desktop.getByRole('button', { name: 'Close', exact: true }).click()
    await expect(selectedNode).toBeFocused()
  })

  test('refreshes retained Debug bytes after clearing history', async ({ page }) => {
    await installObservationFixture(page)
    let retainedBytes = 1_181_116_006
    await page.route('**/api/v1/observations/**', async (route) => {
      const request = route.request()
      const path = new URL(request.url()).pathname.replace('/api/v1', '')
      if (path === '/observations/debug' && request.method() === 'GET') {
        await route.fulfill({
          json: {
            data: {
              enabled: false,
              run_limit_bytes: 67_108_864,
              total_limit_bytes: 2_147_483_648,
              retained_bytes: retainedBytes,
              partial_trace_count: 0,
              retention_days: 7,
            },
          },
        })
        return
      }
      if (path === '/observations/history' && request.method() === 'DELETE') {
        retainedBytes = 0
        await route.fulfill({
          json: { data: { deleted_interactions: 3, deleted_rejections: 0, skipped_active: 0 } },
        })
        return
      }
      await route.fallback()
    })

    await page.goto('/logs')
    await page.getByRole('button', { name: 'Clear history', exact: true }).click()
    await page
      .getByRole('alertdialog', { name: 'Clear history' })
      .getByRole('button', { name: 'Clear history', exact: true })
      .click()
    await page.getByRole('switch', { name: 'Debug' }).click()
    await expect(page.getByRole('alertdialog', { name: 'Enable Debug' })).toContainText('Currently retained: 0 MiB.')
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
