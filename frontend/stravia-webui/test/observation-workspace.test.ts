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

function streamEvent(sequence: number, interactionId: string | null, occurred_at: number): ObservationStreamUpdate {
  return {
    type: 'event',
    event: {
      sequence,
      occurred_at,
      interaction_id: interactionId,
      run_id: 'r1',
      rejection_id: null,
      kind: 'run_finished',
      payload: {},
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
  emit(update: ObservationStreamUpdate): Promise<void>
  calls: { method: string; args: unknown[] }[]
  errors: unknown[]
  focusLatestCalls: number[]
  subscription: { setCursorCalls: number[]; closed: boolean }
  setNow(value: number): void
}

function harness(apiOverrides: Partial<ObservationWorkspaceApi> = {}): Harness {
  const calls: Harness['calls'] = []
  const errors: unknown[] = []
  const focusLatestCalls: number[] = []
  const subscription = { setCursorCalls: [] as number[], closed: false }
  let onUpdate: ((update: ObservationStreamUpdate) => void | Promise<void>) | undefined
  let now = 1_000_000
  let snap: ObservationWorkspaceSnapshot | undefined

  const impl: ObservationWorkspaceApi = {
    forest: () => Promise.resolve(forestPage([root('root1', [summary('i1', { last_event_sequence: 5 })])])),
    failures: () => Promise.resolve({ items: [], total: 0, next_cursor: null, snapshot_sequence: 0 }),
    interaction: (id) => Promise.resolve(detailFor(id)),
    interactionEvents: () => Promise.resolve({ runs: [], snapshot_sequence: 10, next_cursor: null }),
    interactionSummary: (id) =>
      Promise.resolve({ interaction: summary(id), root: root('root1', [summary(id)]), snapshot_sequence: 10 }),
    failure: (_kind, id) => Promise.resolve(failureDetail(id)),
    ...apiOverrides,
  }
  const api: ObservationWorkspaceApi = {
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
    interactionSummary: (id, query) => {
      calls.push({ method: 'interactionSummary', args: [id, query] })
      return impl.interactionSummary(id, query)
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
    snap: () => {
      if (!snap) throw new Error('no snapshot published yet')
      return snap
    },
    emit: async (update) => {
      await onUpdate?.(update)
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
      interactionSummary: (id) =>
        Promise.resolve({
          interaction: summary(id, { last_event_sequence: 6, last_active_at: 999_500 }),
          root: root('root1', [summary(id, { last_event_sequence: 6, last_active_at: 999_500 })], 999_500),
          snapshot_sequence: 11,
        }),
    })
    await h.controller.start()
    await h.emit(streamEvent(6, 'i1', 999_500))
    expect(h.calls.filter((c) => c.method === 'interactionSummary')).toHaveLength(1)
    expect(h.snap().canvasRoots[0].interactions[0].last_event_sequence).toBe(6)
  })

  test('an event already covered by the loaded summary is skipped', async () => {
    const h = harness()
    await h.controller.start()
    await h.emit(streamEvent(3, 'i1', 999_500))
    expect(h.calls.filter((c) => c.method === 'interactionSummary')).toHaveLength(0)
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
    // 选中前到达的块被保留，选中后立即可见；其它交互的块永不露面。
    await h.emit({ type: 'live_content', block: block('b1', 'i1') })
    await h.emit({ type: 'live_content', block: block('b0', 'other') })
    expect(h.snap().selectedLiveBlocks).toEqual([])
    await h.controller.selectInteraction(summary('i1'))
    await h.emit({ type: 'live_content', block: block('b2', 'i1') })
    expect(h.snap().selectedLiveBlocks.map((b) => b.block_id)).toEqual(['b1', 'b2'])
  })

  test('live_gap routes capacity gaps away from save-failure gaps', async () => {
    const h = harness()
    await h.controller.start()
    await h.emit({ type: 'live_gap', interaction_id: 'i1', run_id: 'r1', reason: 'live_capacity' })
    await h.emit({ type: 'live_gap', interaction_id: 'i2', run_id: 'r1', reason: 'missed' })
    expect(h.snap().liveCapacityGaps).toEqual(['i1'])
    expect(h.snap().liveGaps).toEqual(['i2'])
  })
})

describe('selection races', () => {
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
