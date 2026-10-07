import { describe, expect, test } from 'bun:test'
import {
  ObservationWorkspaceController,
  type ObservationWorkspaceApi,
  type ObservationWorkspaceSnapshot,
  type ObservationWorkspaceSubscribe,
} from '../src/lib/observation-workspace'
import type {
  ConfirmedUsage,
  FailedRequestDetail,
  FailedRequestSummary,
  ForestPage,
  ForestRoot,
  InteractionDetail,
  InteractionSummary,
  LiveContentBlock,
  ObservationStreamUpdate,
  ObservationLiveUpdate,
  RootChangesPage,
  InteractionEventsPage,
  RunDetail,
} from '../src/lib/types/observation'

const usage: ConfirmedUsage = {
  input_tokens: 1,
  output_tokens: 2,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  reasoning_tokens: 0,
}

function summary(id: string, extra: Partial<InteractionSummary> = {}): InteractionSummary {
  return {
    context_events: [],
    id,
    root_id: 'root1',
    parent_interaction_id: null,
    generation_root_id: null,
    first_route_id: 'route',
    first_model_display_name: null,
    status: 'completed',
    started_at: 1_000,
    last_active_at: 1_000,
    input_preview: null,
    visible_tail: '',
    failed_request: false,
    client_output_delivered: true,
    usage,
    debug_status: 'none',
    observation_gap: false,
    matched: true,
    last_event_sequence: 1,
    ...extra,
  }
}

function root(id: string, interactions: InteractionSummary[], last_active_at = 1_000): ForestRoot {
  return { id, last_active_at, interactions }
}

function forestPage(roots: ForestRoot[], extra: Partial<ForestPage> = {}): ForestPage {
  return {
    anchor_at: 0,
    window_index: 0,
    window_start: 0,
    window_end: 0,
    roots,
    root_total: roots.length,
    next_cursor: null,
    snapshot_sequence: 10,
    ...extra,
  }
}

function detailFor(id: string, snapshotSequence = 10): InteractionDetail {
  return {
    older_events_cursor: null,
    interaction: summary(id),
    root: root('root1', [summary(id)]),
    runs: [],
    snapshot_sequence: snapshotSequence,
  }
}

function eventRun(sequences: number[]): RunDetail {
  return {
    id: 'r1',
    parent_run_id: null,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: 'route',
    model_display_name: null,
    ingress_protocol: 'openai',
    status: 'completed',
    terminal_reason: null,
    user_interrupted: false,
    debug_enabled: false,
    client_output_committed: true,
    started_at: 1_000,
    finished_at: 2_000,
    delivery_completed_at: 2_000,
    usage,
    trace: null,
    events: sequences.map((sequence) => ({
      sequence,
      occurred_at: 1_000 + sequence,
      interaction_id: 'i1',
      run_id: 'r1',
      rejection_id: null,
      kind: 'platform_tool_finished',
      payload: { name: `tool${sequence}` },
    })),
  }
}

function failureSummary(id: string): FailedRequestSummary {
  return {
    id,
    kind: 'run',
    request_id: `req-${id}`,
    started_at: 2_000,
    duration_ms: 5,
    api_key_id: null,
    api_key_name: null,
    client: null,
    model: null,
    model_display_name: null,
    services: [],
    error: { source: 'upstream', code: 'boom', message: 'boom', status_code: 502, upstream_code: null },
    interaction_id: null,
    root_id: null,
    run_id: null,
    debug_status: 'none',
    observation_gap: false,
  }
}

function failureDetail(id: string): FailedRequestDetail {
  return { request: failureSummary(id), events: [], trace: null, snapshot_sequence: 0 }
}

function streamEvent(
  sequence: number,
  interactionId: string | null,
  occurred_at: number,
  kind = 'run_finished',
): ObservationStreamUpdate {
  return {
    type: 'event',
    event: {
      sequence,
      occurred_at,
      interaction_id: interactionId,
      run_id: 'r1',
      rejection_id: null,
      root_id: interactionId ? 'root1' : null,
      kind,
      boundary: kind === 'run_finished',
    },
  }
}

function block(id: string, interactionId: string, revision = 1): LiveContentBlock {
  return {
    block_id: id,
    interaction_id: interactionId,
    run_id: 'r1',
    kind: 'client_visible_content_delta',
    model_turn_id: null,
    attempt_id: null,
    occurred_at: 1_000,
    revision,
    text: 'delta',
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

interface Harness {
  controller: ObservationWorkspaceController
  snap(): ObservationWorkspaceSnapshot
  emit(update: ObservationStreamUpdate | ObservationLiveUpdate): Promise<void>
  emitLive(scope: number, update: ObservationLiveUpdate): void
  scopes: { id: string; closed: boolean }[]
  advance(ms: number): Promise<void>
  calls: { method: string; args: unknown[] }[]
  errors: unknown[]
  focusLatestCalls: number[]
  subscription: { setCursorCalls: number[]; closed: boolean }
  setNow(value: number): void
  now(): number
}

function harness(apiOverrides: Partial<ObservationWorkspaceApi> = {}): Harness {
  const calls: Harness['calls'] = []
  const errors: unknown[] = []
  const focusLatestCalls: number[] = []
  const subscription = { setCursorCalls: [] as number[], closed: false }
  let onUpdate: ((update: ObservationStreamUpdate) => void | Promise<void>) | undefined
  let now = 1_000_000
  const scopes: Harness['scopes'] = []
  const liveCallbacks: ((update: ObservationLiveUpdate) => void)[] = []
  let snap: ObservationWorkspaceSnapshot | undefined

  const impl: ObservationWorkspaceApi = {
    forest: () => Promise.resolve(forestPage([root('root1', [summary('i1', { last_event_sequence: 5 })], 999_000)])),
    changes: (query) =>
      Promise.resolve({
        snapshot_sequence: 100,
        root_total: 1,
        reset_required: false,
        changes: query.roots.map((item) => ({
          root_id: item.root_id,
          last_active_at: 999_500,
          interactions: [summary('i1', { last_event_sequence: 100 })],
          removed_interaction_ids: [],
          removal_reason: null,
        })),
      }),
    failures: () => Promise.resolve({ items: [], total: 0, next_cursor: null, snapshot_sequence: 100 }),
    interaction: (id) => Promise.resolve(detailFor(id)),
    interactionEvents: () => Promise.resolve({ runs: [], snapshot_sequence: 10, next_cursor: null }),
    failure: (_kind, id) => Promise.resolve(failureDetail(id)),
    ...apiOverrides,
  }
  const api: ObservationWorkspaceApi = {
    changes: (query) => {
      calls.push({ method: 'changes', args: [query] })
      return impl.changes(query)
    },
    forest: (query) => {
      calls.push({ method: 'forest', args: [query] })
      return impl.forest(query)
    },
    failures: (query) => {
      calls.push({ method: 'failures', args: [query] })
      return impl.failures(query)
    },
    interaction: (id, query) => {
      calls.push({ method: 'interaction', args: [id, query] })
      return impl.interaction(id, query)
    },
    interactionEvents: (id, query) => {
      calls.push({ method: 'interactionEvents', args: [id, query] })
      return impl.interactionEvents(id, query)
    },
    failure: (kind, id) => {
      calls.push({ method: 'failure', args: [kind, id] })
      return impl.failure(kind, id)
    },
  }

  const subscribe: ObservationWorkspaceSubscribe = (_sequence, update) => {
    onUpdate = update
    return {
      close: () => {
        subscription.closed = true
      },
      setCursor: (sequence) => {
        subscription.setCursorCalls.push(sequence)
      },
    }
  }

  const controller = new ObservationWorkspaceController({
    api,
    subscribe,
    subscribeLive: (id, update) => {
      const scope = { id, closed: false }
      scopes.push(scope)
      liveCallbacks.push(update)
      return {
        close: () => {
          scope.closed = true
        },
        setCursor: () => undefined,
      }
    },
    hooks: {
      focusLatest: () => {
        focusLatestCalls.push(1)
      },
      focusNode: () => undefined,
      fitAfterAllLoaded: () => undefined,
      onError: (error) => errors.push(error),
    },
    onSnapshot: (snapshot) => {
      snap = snapshot
    },
    now: () => now,
  })

  return {
    controller,
    now: () => now,
    snap: () => {
      if (!snap) throw new Error('no snapshot published yet')
      return snap
    },
    emit: async (update) => {
      if (update.type.startsWith('live_')) liveCallbacks.at(-1)?.(update as ObservationLiveUpdate)
      else await onUpdate?.(update as ObservationStreamUpdate)
      for (let i = 0; i < 12; i += 1) await Promise.resolve()
    },
    emitLive: (scope, update) => liveCallbacks[scope]?.(update),
    scopes,
    advance: async (ms) => {
      now += ms
      controller.advanceClock()
      for (let i = 0; i < 12; i += 1) await Promise.resolve()
    },
    calls,
    errors,
    focusLatestCalls,
    subscription,
    setNow: (value) => {
      now = value
    },
  }
}

describe('start and forest paging', () => {
  test('start loads the forest, opens the stream at the page snapshot, and publishes roots', async () => {
    const h = harness()
    await h.controller.start()
    expect(h.calls.map((c) => c.method)).toEqual(['forest'])
    expect(h.snap().canvasRoots.map((r) => r.id)).toEqual(['root1'])
    expect(h.snap().loading).toBe(false)
    expect(h.snap().liveWindow).toBe(true)
  })

  test('forest failure lands on loadError instead of throwing', async () => {
    const h = harness({ forest: () => Promise.reject(new Error('offline')) })
    await h.controller.start()
    expect(h.snap().loadError).toBeInstanceOf(Error)
    expect(h.snap().loading).toBe(false)
  })

  test('stale forest responses are discarded after a window change', async () => {
    const first = deferred<ForestPage>()
    const second = deferred<ForestPage>()
    let call = 0
    const h = harness({
      forest: () => {
        call += 1
        return call === 1 ? first.promise : second.promise
      },
    })
    const started = h.controller.start()
    // 窗口切换抬 rangeVersion；第一页迟到后不得覆盖第二页。
    const changed = h.controller.changeWindow(1)
    first.resolve(forestPage([root('stale', [summary('old')])]))
    second.resolve(forestPage([root('fresh', [summary('new')])]))
    await Promise.all([started, changed])
    expect(h.snap().canvasRoots.map((r) => r.id)).toEqual(['fresh'])
    expect(h.snap().liveWindow).toBe(false)
  })
})

describe('stream updates', () => {
  test('a known event newer than the summary refetches and reconciles its root', async () => {
    const h = harness({
      changes: () =>
        Promise.resolve({
          changes: [
            {
              root_id: 'root1',
              last_active_at: 999_500,
              interactions: [summary('i1', { last_event_sequence: 6, last_active_at: 999_500 })],
              removed_interaction_ids: [],
              removal_reason: null,
            },
          ],
          root_total: 1,
          reset_required: false,
          snapshot_sequence: 11,
        }),
    })
    await h.controller.start()
    await h.emit(streamEvent(6, 'i1', 999_500))
    expect(h.calls.filter((c) => c.method === 'changes')).toHaveLength(1)
    expect(h.snap().canvasRoots[0].interactions[0].last_event_sequence).toBe(6)
  })

  test('an event already covered by the loaded summary is skipped', async () => {
    const h = harness()
    await h.controller.start()
    await h.emit(streamEvent(3, 'i1', 999_500))
    expect(h.calls.filter((c) => c.method === 'changes')).toHaveLength(0)
  })

  test('reset_required clears live state, refetches, and advances the stream cursor', async () => {
    const h = harness()
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    await h.emit({ type: 'live_gap', interaction_id: 'i1', run_id: 'r1', reason: 'missed' })
    expect(h.snap().liveGaps).toEqual(['i1'])
    await h.emit({ type: 'reset_required', snapshot_sequence: 10 })
    expect(h.snap().liveGaps).toEqual([])
    expect(h.snap().interactionDetail?.interaction.id).toBe('i1')
    expect(h.calls.map((c) => c.method)).toEqual([
      'forest',
      'interaction',
      'interactionEvents',
      'forest',
      'interaction',
    ])
    expect(h.subscription.setCursorCalls).toEqual([10])
  })

  test('live_content is retained per interaction and only the selected one surfaces', async () => {
    const h = harness()
    await h.controller.start()
    // 无scope不接收正文；其它交互的块永不露面。
    await h.emit({ type: 'live_content', block: block('b1', 'i1') })
    await h.emit({ type: 'live_content', block: block('b0', 'other') })
    expect(h.snap().selectedLiveBlocks).toEqual([])
    await h.controller.selectInteraction(summary('i1'))
    await h.emit({ type: 'live_content', block: block('b2', 'i1') })
    expect(h.snap().selectedLiveBlocks.map((b) => b.block_id)).toEqual(['b2'])
  })

  test('follow only reacts to events that extend the output preview', async () => {
    const h = harness()
    await h.controller.start()
    // 跟随中：思考增量与工具调用不移动视口，只有输出预览追加触发 focusLatest。
    const focused = h.focusLatestCalls.length
    await h.emit(streamEvent(6, 'i1', 999_500, 'model_thinking'))
    await h.emit(streamEvent(7, 'i1', 999_501, 'platform_tool_finished'))
    expect(h.focusLatestCalls).toHaveLength(focused)
    await h.emit(streamEvent(8, 'i1', 999_502, 'client_visible_content'))
    expect(h.focusLatestCalls).toHaveLength(focused + 1)
    // 暂停后同理：非预览事件不点亮「新活动·跟随」。
    h.controller.pauseFollow()
    await h.emit(streamEvent(9, 'i1', 999_503, 'model_thinking'))
    await h.emit(streamEvent(10, 'i1', 999_504, 'client_tool_result'))
    expect(h.snap().hasNewActivity).toBe(false)
    await h.emit(streamEvent(11, 'i1', 999_505, 'client_visible_content'))
    expect(h.snap().hasNewActivity).toBe(true)
  })

  test('live_gap routes capacity gaps away from save-failure gaps', async () => {
    const h = harness()
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    await h.emit({ type: 'live_gap', interaction_id: 'i1', run_id: 'r1', reason: 'live_capacity' })
    await h.emit({ type: 'live_gap', interaction_id: 'i2', run_id: 'r1', reason: 'missed' })
    expect(h.snap().liveCapacityGaps).toEqual(['i1'])
    expect(h.snap().liveGaps).toEqual([])
  })
})

describe('selection races', () => {
  test('scope snapshots sync directly and closed or superseded scopes cannot supply body', async () => {
    const h = harness()
    await h.controller.start()
    expect(h.scopes).toEqual([])
    await h.controller.selectInteraction(summary('i1', { status: 'running' }))
    const epoch = h.snap().liveContentEpoch
    await h.emit({ type: 'live_snapshot', blocks: [block('a', 'i1')] })
    expect(h.snap().liveContentEpoch).toBe(epoch + 1)
    await h.controller.selectInteraction(summary('i2'))
    expect(h.scopes[0].closed).toBe(true)
    h.emitLive(0, { type: 'live_content', block: block('late', 'i1') })
    expect(h.snap().selectedLiveBlocks).toEqual([])
    await h.emit({ type: 'live_content', block: block('b', 'i2', 2) })
    await h.emit({ type: 'live_content', block: block('b', 'i2', 1) })
    expect(h.snap().selectedLiveBlocks[0].revision).toBe(2)
    await h.emit({ type: 'live_finished', interaction_id: 'i2', run_id: 'r1' })
    const terminalEpoch = h.snap().liveTerminalEpoch
    await h.emit({ type: 'live_finished', interaction_id: 'i2', run_id: 'r1' })
    expect(h.snap().liveTerminalEpoch).toBe(terminalEpoch)
    await h.emit({ type: 'live_content', block: block('b', 'i2', 3) })
    expect(h.snap().selectedLiveActive).toBe(false)
    expect(h.snap().selectedLiveBlocks[0].block_id).toBe('b')
    h.controller.closeInspector()
    h.emitLive(1, { type: 'live_snapshot', blocks: [block('b', 'i2')] })
    expect(h.snap().selectedLiveBlocks).toEqual([])
    expect(h.scopes[1].closed).toBe(true)
    h.controller.dispose()
  })
  test('a stale selection response cannot overwrite the newer selection', async () => {
    const first = deferred<InteractionDetail>()
    const second = deferred<InteractionDetail>()
    const h = harness({ interaction: (id) => (id === 'i1' ? first.promise : second.promise) })
    await h.controller.start()
    const p1 = h.controller.selectInteraction(summary('i1'))
    const p2 = h.controller.selectInteraction(summary('i2'))
    first.resolve(detailFor('i1'))
    second.resolve(detailFor('i2'))
    await Promise.all([p1, p2])
    expect(h.snap().selectedInteraction?.id).toBe('i2')
    expect(h.snap().interactionDetail?.interaction.id).toBe('i2')
  })

  test('closing the inspector invalidates an in-flight selection', async () => {
    const pending = deferred<InteractionDetail>()
    const h = harness({ interaction: () => pending.promise })
    await h.controller.start()
    const p = h.controller.selectInteraction(summary('i1'))
    h.controller.closeInspector()
    pending.resolve(detailFor('i1'))
    await p
    expect(h.snap().selectedInteraction).toBeUndefined()
    expect(h.snap().interactionDetail).toBeUndefined()
  })
})

describe('bounded refresh orchestration', () => {
  const changePage = (sequence: number, text: string): RootChangesPage => ({
    snapshot_sequence: sequence,
    root_total: 1,
    reset_required: false,
    changes: [
      {
        root_id: 'root1',
        last_active_at: 999_500,
        interactions: [summary('i1', { last_event_sequence: sequence, visible_tail: text })],
        removed_interaction_ids: [],
        removal_reason: null,
      },
    ],
  })

  test('delta updates keep loaded root and sibling positions while new members append', async () => {
    const h = harness({
      forest: () =>
        Promise.resolve(
          forestPage([
            root('root1', [summary('first'), summary('i1'), summary('last')], 999_500),
            root('root2', [summary('other', { root_id: 'root2' })], 999_500),
          ]),
        ),
      changes: () =>
        Promise.resolve({
          ...changePage(11, 'updated'),
          root_total: 2,
          changes: [
            {
              ...changePage(11, 'updated').changes[0],
              interactions: [
                summary('new', { last_event_sequence: 11 }),
                summary('i1', { last_event_sequence: 11, visible_tail: 'updated' }),
              ],
            },
          ],
        }),
    })
    await h.controller.start()
    await h.emit(streamEvent(11, 'i1', 999_500))
    expect(h.snap().canvasRoots.map((root) => root.id)).toEqual(['root1', 'root2'])
    expect(h.snap().canvasRoots[0].interactions.map((item) => item.id)).toEqual(['first', 'i1', 'last', 'new'])
    expect(h.snap().canvasRoots[0].interactions[1].visible_tail).toBe('updated')
    h.controller.dispose()
  })

  test('continuous same-root notifications share the first fixed deadline', async () => {
    let requestedAt = 0
    const h = harness({
      changes: () => {
        requestedAt = h.now()
        return Promise.resolve(changePage(15, 'latest'))
      },
    })
    await h.controller.start()
    const startedAt = h.now()
    await h.emit(streamEvent(11, 'i1', 999_500, 'model_thinking'))
    await h.advance(60)
    await h.emit(streamEvent(12, 'sibling', 999_500, 'platform_tool_finished'))
    await h.advance(39)
    expect(h.calls.filter((call) => call.method === 'changes')).toHaveLength(0)
    await h.emit(streamEvent(15, 'i1', 999_500, 'model_thinking'))
    await h.advance(1)
    expect(h.calls.filter((call) => call.method === 'changes')).toHaveLength(1)
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('latest')
    expect(requestedAt - startedAt).toBe(100)
    console.log(`frontend_merge_wait_ms=${requestedAt - startedAt}`)
    h.controller.dispose()
  })

  test('reset completion waits for authoritative queries and failed rebuild exposes recovery without losing detail', async () => {
    const pending = deferred<ForestPage>()
    let forestCalls = 0
    const h = harness({
      forest: () =>
        ++forestCalls === 1 ? Promise.resolve(forestPage([root('root1', [summary('i1')], 999_500)])) : pending.promise,
    })
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    let completed = false
    const reset = h.emit({ type: 'reset_required', snapshot_sequence: 11 }).then(
      () => {
        completed = true
      },
      (error) => {
        completed = true
        throw error
      },
    )
    await Promise.resolve()
    expect(completed).toBe(false)
    expect(h.subscription.setCursorCalls).toEqual([])
    pending.reject(new Error('snapshot unavailable'))
    const error: unknown = await reset.catch((reason: unknown) => reason)
    expect(error).toBeInstanceOf(Error)
    expect((error as Error).message).toBe('snapshot unavailable')
    expect(h.snap().interactionDetail?.interaction.id).toBe('i1')
    expect(h.snap().canvasRoots[0].interactions[0].id).toBe('i1')
    expect(h.snap().loadError).toBeInstanceOf(Error)
    expect(h.subscription.setCursorCalls).toEqual([])
    h.controller.dispose()
  })

  test('reset accepts a cleared forest watermark instead of retaining ghost roots', async () => {
    let calls = 0
    const stale = deferred<RootChangesPage>()
    const h = harness({
      changes: () => stale.promise,
      forest: () =>
        Promise.resolve(
          ++calls === 1
            ? forestPage([root('root1', [summary('i1', { last_event_sequence: 20 })], 999_500)], {
                snapshot_sequence: 20,
              })
            : forestPage([], { snapshot_sequence: 0 }),
        ),
    })
    await h.controller.start()
    await h.emit(streamEvent(21, 'i1', 999_500))
    await h.controller.refreshData()
    stale.resolve(changePage(21, 'deleted stale body'))
    await h.advance(0)
    expect(h.snap().canvasRoots).toEqual([])
    expect(h.snap().rootTotal).toBe(0)
    expect(h.subscription.setCursorCalls).toEqual([0])
    h.controller.dispose()
  })

  test('boundary notifications during an external rebuild stay pending without restarting recovery', async () => {
    const rebuild = deferred<ForestPage>()
    let forests = 0
    const initial = forestPage([root('root1', [summary('i1', { last_event_sequence: 20 })], 999_500)], {
      snapshot_sequence: 20,
    })
    const h = harness({
      forest: () => (++forests === 1 ? Promise.resolve(initial) : rebuild.promise),
      changes: () => Promise.resolve(changePage(23, 'latest after recovery')),
    })
    await h.controller.start()
    const recovery = h.controller.refreshData()
    await h.emit(streamEvent(21, 'i1', 999_501))
    await h.advance(100)
    await h.emit(streamEvent(22, 'i1', 999_502))
    await h.advance(100)
    await h.emit(streamEvent(23, 'i1', 999_503))
    expect(forests).toBe(2)
    expect(h.calls.filter((call) => call.method === 'changes')).toHaveLength(0)
    rebuild.resolve(initial)
    await recovery
    await h.advance(0)
    expect(forests).toBe(2)
    expect(h.calls.filter((call) => call.method === 'changes')).toHaveLength(1)
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('latest after recovery')
    h.controller.dispose()
  })

  test('lower-watermark reset rebuilds a surviving selection and replaces its body scope', async () => {
    let forestCalls = 0
    let detailCalls = 0
    const h = harness({
      forest: () =>
        Promise.resolve(
          forestPage([root('root1', [summary('i1')], 999_500)], { snapshot_sequence: ++forestCalls === 1 ? 20 : 0 }),
        ),
      interaction: () => Promise.resolve(detailFor('i1', ++detailCalls === 1 ? 20 : 0)),
    })
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    await h.emit({ type: 'live_content', block: block('old-body', 'i1') })
    await h.emit({ type: 'reset_required', snapshot_sequence: 0 })
    expect(h.snap().interactionDetail?.snapshot_sequence).toBe(0)
    expect(h.scopes.map((scope) => scope.closed)).toEqual([true, false])
    await h.emit({ type: 'live_snapshot', blocks: [] })
    expect(h.snap().selectedLiveBlocks).toEqual([])
    h.controller.dispose()
  })

  test('reset detail HTTP 503 retains readable data and inspector retry rebuilds the complete scope', async () => {
    let details = 0
    const unavailable = Object.assign(new Error('detail unavailable'), { status: 503 })
    const h = harness({
      interaction: () =>
        ++details === 2 ? Promise.reject(unavailable) : Promise.resolve(detailFor('i1', details === 1 ? 20 : 0)),
    })
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    const error: unknown = await h
      .emit({ type: 'reset_required', snapshot_sequence: 0 })
      .catch((reason: unknown) => reason)
    expect(error).toBe(unavailable)
    expect(h.snap().detailError).toBe(unavailable)
    expect(h.snap().interactionDetail?.snapshot_sequence).toBe(20)
    expect(h.scopes.map((scope) => scope.closed)).toEqual([true])
    expect(h.subscription.setCursorCalls).toEqual([])
    await h.controller.refreshSelectedDetail()
    expect(h.snap().detailError).toBeUndefined()
    expect(h.snap().loadError).toBeUndefined()
    expect(h.snap().interactionDetail?.snapshot_sequence).toBe(0)
    expect(h.scopes.map((scope) => scope.closed)).toEqual([true, false])
    expect(h.subscription.setCursorCalls).toEqual([10])
    h.controller.dispose()
  })

  test('reset failure-detail errors retain diagnostics and the same inspector retry restores them', async () => {
    let details = 0
    const unavailable = Object.assign(new Error('failure diagnostics unavailable'), { status: 503 })
    const h = harness({
      failure: () => (++details === 2 ? Promise.reject(unavailable) : Promise.resolve(failureDetail('f1'))),
    })
    await h.controller.start()
    await h.controller.selectFailure(failureSummary('f1'))
    const error: unknown = await h
      .emit({ type: 'reset_required', snapshot_sequence: 0 })
      .catch((reason: unknown) => reason)
    expect(error).toBe(unavailable)
    expect(h.snap().failureDetailError).toBe(unavailable)
    expect(h.snap().failureDetail?.request.id).toBe('f1')
    expect(h.subscription.setCursorCalls).toEqual([])
    await h.controller.refreshSelectedDetail()
    expect(h.snap().failureDetailError).toBeUndefined()
    expect(h.snap().loadError).toBeUndefined()
    expect(h.snap().failureDetail?.request.id).toBe('f1')
    expect(h.subscription.setCursorCalls).toEqual([10])
    h.controller.dispose()
  })

  test('a live root that advances beyond the client clock while querying remains visible', async () => {
    const pending = deferred<RootChangesPage>()
    const h = harness({ changes: () => pending.promise })
    await h.controller.start()
    await h.emit(streamEvent(11, 'i1', 999_500))
    await h.advance(50)
    pending.resolve({
      ...changePage(11, 'server-ahead final'),
      changes: [
        {
          ...changePage(11, 'server-ahead final').changes[0],
          last_active_at: 1_050_000,
          interactions: [
            summary('i1', { last_active_at: 1_050_000, last_event_sequence: 11, visible_tail: 'server-ahead final' }),
          ],
        },
      ],
    })
    await h.advance(0)
    expect(h.snap().canvasRoots[0].last_active_at).toBeGreaterThan(h.snap().windowEnd)
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('server-ahead final')
    expect(h.snap().migratedRoots.size).toBe(0)
    h.controller.dispose()
  })

  test('a reset detail HTTP 404 retires deleted selection and stops its body scope', async () => {
    let interactions = 0
    let forestCalls = 0
    const h = harness({
      forest: () =>
        Promise.resolve(
          ++forestCalls === 1
            ? forestPage([root('root1', [summary('i1')], 999_500)])
            : forestPage([], { snapshot_sequence: 11 }),
        ),
      interaction: () =>
        ++interactions === 1
          ? Promise.resolve(detailFor('i1'))
          : Promise.reject(Object.assign(new Error('not found'), { status: 404 })),
    })
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    await h.emit({ type: 'live_content', block: block('deleted-body', 'i1') })
    await h.emit({ type: 'reset_required', snapshot_sequence: 11 })
    expect(h.snap().selectedInteraction).toBeUndefined()
    expect(h.snap().interactionDetail).toBeUndefined()
    expect(h.snap().selectedLiveBlocks).toEqual([])
    expect(h.snap().canvasRoots).toEqual([])
    expect(h.scopes.every((scope) => scope.closed)).toBe(true)
    expect(h.subscription.setCursorCalls).toEqual([11])
    h.controller.dispose()
  })

  test('filter removal keeps readable selection but deleted root closes the inspector', async () => {
    let calls = 0
    const h = harness({
      interactionEvents: () => Promise.resolve({ runs: [], snapshot_sequence: 100, next_cursor: null }),
      changes: () =>
        Promise.resolve({
          snapshot_sequence: 11 + calls++,
          root_total: 0,
          reset_required: false,
          changes: [
            {
              root_id: 'root1',
              last_active_at: 999_500,
              interactions: [],
              removed_interaction_ids: ['i1'],
              removal_reason: calls === 1 ? 'filter' : 'deleted',
            },
          ],
        }),
    })
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    await h.emit(streamEvent(11, 'i1', 999_500))
    expect(h.snap().canvasRoots).toEqual([])
    expect(h.snap().interactionDetail?.interaction.id).toBe('i1')
    expect(h.scopes[0].closed).toBe(false)
    await h.emit(streamEvent(12, 'i1', 999_500))
    expect(h.snap().interactionDetail).toBeUndefined()
    expect(h.snap().selectedInteraction).toBeUndefined()
    expect(h.scopes[0].closed).toBe(true)
    h.controller.dispose()
  })

  test('selected incremental pagination fixes its upper bound and retains new in-flight notifications', async () => {
    const page = deferred<InteractionEventsPage>()
    let calls = 0
    const h = harness({
      interaction: () => Promise.resolve({ ...detailFor('i1'), runs: [eventRun([10])] }),
      interactionEvents: (_id, query) => {
        calls += 1
        if (calls === 1) return Promise.resolve({ runs: [], snapshot_sequence: 10, next_cursor: null })
        if (calls === 2) return page.promise
        if (query.through_sequence === 12)
          return Promise.resolve({ runs: [eventRun([12])], snapshot_sequence: 12, next_cursor: null })
        return Promise.resolve({ runs: [eventRun([13])], snapshot_sequence: 13, next_cursor: null })
      },
    })
    await h.controller.start()
    await h.controller.selectInteraction(summary('i1'))
    await h.emit(streamEvent(11, 'i1', 999_500))
    await h.emit(streamEvent(13, 'i1', 999_500))
    expect(calls).toBe(2)
    page.resolve({ runs: [eventRun([11])], snapshot_sequence: 12, next_cursor: 11 })
    await h.advance(0)
    expect(h.snap().interactionDetail?.runs[0].events.map((event) => event.sequence)).toEqual([10, 11, 12])
    expect(h.snap().interactionDetail?.snapshot_sequence).toBe(12)
    await h.advance(100)
    expect(h.snap().interactionDetail?.runs[0].events.map((event) => event.sequence)).toEqual([10, 11, 12, 13])
    expect(h.snap().interactionDetail?.snapshot_sequence).toBe(13)
    h.controller.dispose()
  })

  test('a boundary flushes immediately but an in-flight response clears only covered notifications', async () => {
    const first = deferred<RootChangesPage>()
    const second = deferred<RootChangesPage>()
    let calls = 0
    const h = harness({ changes: () => (++calls === 1 ? first.promise : second.promise) })
    await h.controller.start()
    await h.emit(streamEvent(11, 'i1', 999_500))
    await h.emit(streamEvent(12, 'i1', 999_501))
    expect(calls).toBe(1)
    first.resolve(changePage(11, 'partial'))
    await h.advance(0)
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('partial')
    await h.advance(100)
    expect(calls).toBe(2)
    second.resolve(changePage(12, 'final'))
    await h.advance(0)
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('final')
    h.controller.dispose()
  })

  test('failed refresh preserves usable summaries and recover action covers pending notifications', async () => {
    let calls = 0
    const h = harness({
      changes: () =>
        ++calls === 1 ? Promise.reject(new Error('offline')) : Promise.resolve(changePage(12, 'recovered')),
    })
    await h.controller.start()
    await h.emit(streamEvent(12, 'i1', 999_500))
    expect(h.snap().loadError).toBeInstanceOf(Error)
    expect(h.snap().canvasRoots[0].interactions[0].last_event_sequence).toBe(5)
    await h.controller.reloadForest()
    await h.advance(0)
    expect(h.snap().loadError).toBeUndefined()
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('recovered')
    h.controller.dispose()
  })

  test('late delta from a former filter epoch cannot restore its members', async () => {
    const pending = deferred<RootChangesPage>()
    let forestCalls = 0
    const h = harness({
      changes: () => pending.promise,
      forest: () =>
        Promise.resolve(++forestCalls === 1 ? forestPage([root('root1', [summary('i1')], 999_500)]) : forestPage([])),
    })
    await h.controller.start()
    await h.emit(streamEvent(11, 'i1', 999_500))
    h.controller.setProviderFilter('new')
    await h.controller.applyFilters()
    pending.resolve(changePage(11, 'stale'))
    await h.advance(0)
    expect(h.snap().canvasRoots).toEqual([])
    h.controller.dispose()
  })

  test('delta keeps unchanged context and honors changed matched flags and explicit filter removal', async () => {
    let calls = 0
    const h = harness({
      forest: () =>
        Promise.resolve(
          forestPage([
            root(
              'root1',
              [summary('i1'), summary('sibling', { matched: false }), summary('context', { matched: false })],
              999_500,
            ),
          ]),
        ),
      changes: () =>
        Promise.resolve(
          ++calls === 1
            ? {
                ...changePage(11, 'new'),
                changes: [
                  {
                    ...changePage(11, 'new').changes[0],
                    interactions: [
                      summary('i1', { matched: false, last_event_sequence: 11 }),
                      summary('sibling', { matched: true }),
                    ],
                  },
                ],
              }
            : {
                ...changePage(12, ''),
                root_total: 0,
                changes: [
                  {
                    root_id: 'root1',
                    last_active_at: 999_500,
                    interactions: [],
                    removed_interaction_ids: ['i1', 'sibling', 'context'],
                    removal_reason: 'filter',
                  },
                ],
              },
        ),
    })
    await h.controller.start()
    await h.emit(streamEvent(11, 'i1', 999_500))
    expect(h.snap().canvasRoots[0].interactions.map((item) => [item.id, item.matched])).toEqual([
      ['i1', false],
      ['sibling', true],
      ['context', false],
    ])
    await h.emit(streamEvent(12, 'i1', 999_500))
    expect(h.snap().canvasRoots).toEqual([])
    expect(h.snap().rootTotal).toBe(0)
    h.controller.dispose()
  })

  test('historical window removal preserves the loaded root until physical deletion', async () => {
    let calls = 0
    const h = harness({
      changes: () =>
        Promise.resolve({
          snapshot_sequence: ++calls + 10,
          root_total: 0,
          reset_required: false,
          changes: [
            {
              root_id: 'root1',
              last_active_at: 999_500,
              interactions:
                calls === 1 ? [summary('i1', { last_event_sequence: 11, visible_tail: 'migrated final' })] : [],
              removed_interaction_ids: calls === 1 ? [] : ['i1'],
              removal_reason: calls === 1 ? 'window' : 'deleted',
            },
          ],
        }),
    })
    await h.controller.start()
    await h.controller.changeWindow(1)
    await h.emit(streamEvent(11, 'i1', 999_500))
    expect(h.snap().canvasRoots[0].interactions.map((item) => item.id)).toEqual(['i1'])
    expect(h.snap().canvasRoots[0].interactions[0].visible_tail).toBe('migrated final')
    expect(h.snap().migratedRoots.has('root1')).toBe(true)
    await h.emit(streamEvent(12, 'i1', 999_500))
    expect(h.snap().canvasRoots).toEqual([])
    h.controller.dispose()
  })

  test('failures tab merges terminal notifications without querying hidden canvas roots', async () => {
    const first = deferred<import('../src/lib/types/observation').FailedRequestPage>()
    let failures = 0
    const h = harness({
      failures: () =>
        ++failures === 1
          ? Promise.resolve({ items: [], total: 0, next_cursor: null, snapshot_sequence: 10 })
          : failures === 2
            ? first.promise
            : Promise.resolve({
                items: [failureSummary('failed')],
                total: 1,
                next_cursor: null,
                snapshot_sequence: 12,
              }),
    })
    await h.controller.start()
    await h.controller.tabChanged('failures')
    await h.emit(streamEvent(11, 'i1', 999_500))
    await h.emit(streamEvent(12, 'other', 999_500))
    expect(failures).toBe(2)
    expect(h.calls.filter((call) => call.method === 'changes')).toHaveLength(0)
    first.resolve({ items: [], total: 0, next_cursor: null, snapshot_sequence: 11 })
    await h.advance(0)
    await h.advance(100)
    expect(h.snap().failures.map((item) => item.id)).toEqual(['failed'])
    expect(h.snap().failureTotal).toBe(1)
    h.controller.dispose()
  })
})

describe('live window clock', () => {
  test('advanceClock reloads the forest when loaded roots aged out of the window', async () => {
    const h = harness({ forest: () => Promise.resolve(forestPage([root('old', [summary('i1')], 500_000)])) })
    await h.controller.start()
    expect(h.calls.filter((c) => c.method === 'forest')).toHaveLength(1)
    h.setNow(1_200_000)
    h.controller.advanceClock()
    await Promise.resolve()
    await Promise.resolve()
    expect(h.calls.filter((c) => c.method === 'forest')).toHaveLength(2)
    expect(h.snap().windowStart).toBe(600_000)
  })

  test('advanceClock is a no-op outside the live window', async () => {
    const h = harness()
    await h.controller.start()
    await h.controller.changeWindow(1)
    h.controller.advanceClock()
    await Promise.resolve()
    expect(h.calls.filter((c) => c.method === 'forest')).toHaveLength(2)
  })
})

describe('failures paging', () => {
  test('replace resets the list and load-more appends through the cursor', async () => {
    const h = harness({
      failures: (query) =>
        Promise.resolve(
          query.cursor === undefined
            ? { items: [failureSummary('f1')], total: 2, next_cursor: 'c2', snapshot_sequence: 0 }
            : { items: [failureSummary('f2')], total: 2, next_cursor: null, snapshot_sequence: 0 },
        ),
    })
    await h.controller.loadFailures()
    await h.controller.loadFailures(false)
    expect(h.snap().failures.map((f) => f.id)).toEqual(['f1', 'f2'])
    expect(h.snap().failureCursor).toBeNull()
    const queries = h.calls.filter((c) => c.method === 'failures').map((c) => c.args[0] as { cursor?: string })
    expect(queries[0].cursor).toBeUndefined()
    expect(queries[1].cursor).toBe('c2')
  })
})

describe('hidden failure reveal', () => {
  const hidden = () => summary('hidden', { failed_request: true, client_output_delivered: false, status: 'completed' })

  test('a failed-and-undelivered interaction stays off the canvas until revealed', async () => {
    const h = harness({ forest: () => Promise.resolve(forestPage([root('root1', [hidden()])])) })
    await h.controller.start()
    expect(h.snap().canvasRoots).toEqual([])
  })

  test('deep-linking reveals the hidden interaction and selects it', async () => {
    const h = harness({
      forest: () => Promise.resolve(forestPage([root('root1', [hidden()])])),
      interaction: (id) =>
        Promise.resolve({ ...detailFor(id), interaction: hidden(), root: root('root1', [hidden()]) }),
    })
    await h.controller.start()
    await h.controller.deepLinkInteraction('hidden')
    expect(h.snap().selectedInteraction?.id).toBe('hidden')
    expect(
      h
        .snap()
        .canvasRoots.flatMap((r) => r.interactions)
        .map((i) => i.id),
    ).toEqual(['hidden'])
    expect(h.snap().followPaused).toBe(true)
  })
})

describe('selected path', () => {
  test('walks confirmed ancestors through the shared parent-edge index', async () => {
    const chain = [
      summary('i1', { last_event_sequence: 5 }),
      summary('i2', { parent_interaction_id: 'i1', started_at: 2_000 }),
      summary('i3', { parent_interaction_id: 'i2', started_at: 3_000 }),
    ]
    const h = harness({
      forest: () => Promise.resolve(forestPage([root('root1', chain)])),
      // detail 响应须返回链内节点本身；默认 detailFor 的 summary 没有 parent，会截断路径。
      interaction: (id) =>
        Promise.resolve({ ...detailFor(id), interaction: chain.find((item) => item.id === id) ?? summary(id) }),
    })
    await h.controller.start()
    await h.controller.selectInteraction(chain[2])
    expect([...h.snap().selectedPath].sort()).toEqual(['i1', 'i2', 'i3'])
    await h.controller.selectInteraction(chain[0])
    expect([...h.snap().selectedPath]).toEqual(['i1'])
  })
})
