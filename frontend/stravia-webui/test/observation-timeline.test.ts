import { describe, expect, test } from 'bun:test'
import { computeTps, formatDuration, formatTime } from '../src/lib/format'
import { deriveTimeline, itemEvents, itemKey, runMetrics } from '../src/lib/observation-timeline'
import { attemptTpsInput, observationEventSummary } from '../src/lib/observation-event-summary'
import * as m from '../src/lib/paraglide/messages.js'
import type {
  ConfirmedUsage,
  FailedRequestDetail,
  InteractionDetail,
  ObservationEvent,
  RunDetail,
} from '../src/lib/types/observation'

const usage: ConfirmedUsage = {
  input_tokens: 10,
  output_tokens: 20,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  reasoning_tokens: 0,
}

function event(sequence: number, kind: string, payload: unknown = {}, occurred_at = sequence): ObservationEvent {
  return { sequence, occurred_at, interaction_id: 'i1', run_id: 'r1', rejection_id: null, kind, payload }
}

function run(
  id: string,
  started_at: number,
  events: ObservationEvent[] = [],
  extra: Partial<RunDetail> = {},
): RunDetail {
  return {
    id,
    parent_run_id: null,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: `route-${id}`,
    model_display_name: null,
    ingress_protocol: 'test',
    status: 'completed',
    terminal_reason: null,
    user_interrupted: false,
    debug_enabled: false,
    client_output_committed: true,
    started_at,
    finished_at: null,
    delivery_completed_at: null,
    usage,
    events,
    trace: null,
    ...extra,
  }
}

function detail(runs: RunDetail[], started_at = 0): InteractionDetail {
  return {
    older_events_cursor: null,
    interaction: {
      context_events: [],
      id: 'i1',
      root_id: 'root1',
      parent_interaction_id: null,
      generation_root_id: null,
      first_route_id: 'first-route',
      first_model_display_name: null,
      status: 'completed',
      started_at,
      last_active_at: started_at,
      input_preview: null,
      visible_tail: '',
      failed_request: false,
      client_output_delivered: true,
      usage,
      debug_status: 'none',
      observation_gap: false,
      matched: true,
      last_event_sequence: 0,
    },
    root: { id: 'root1', last_active_at: started_at, interactions: [] },
    runs,
    snapshot_sequence: 0,
  }
}

function failureDetail(events: ObservationEvent[]): FailedRequestDetail {
  return {
    request: {
      id: 'f1',
      kind: 'rejection',
      request_id: 'req1',
      started_at: 10_000,
      duration_ms: 5,
      api_key_id: null,
      api_key_name: null,
      client: null,
      model: null,
      model_display_name: null,
      services: [],
      error: { source: 'upstream', code: 'upstream_timeout', message: 'boom', status_code: 504, upstream_code: null },
      interaction_id: null,
      root_id: null,
      run_id: null,
      debug_status: 'none',
      observation_gap: false,
    },
    events,
    trace: null,
    snapshot_sequence: 0,
  }
}

describe('terminal lifecycle revisions', () => {
  test('keeps diagnostic chronology without adding revision usage to the run projection or TPS', () => {
    const initial = event(1, 'target_attempt_finished', {
      attempt_id: 'attempt',
      status: 'failed',
      duration_ms: 2000,
      first_token_ms: 1000,
      usage: { ...usage, output_tokens: 10 },
    })
    const revised = event(3, 'target_attempt_finished', {
      attempt_id: 'attempt',
      status: 'failed',
      duration_ms: 2000,
      first_token_ms: 1000,
      usage: { ...usage, output_tokens: 20 },
    })
    const delivery = event(2, 'run_finished', { status: 'failed', delivery: null })
    const delivered = event(4, 'run_finished', {
      status: 'failed',
      delivery: { status: 'delivery_failed', reason: 'connection_closed', completed_at: 4 },
    })
    const current = run('r1', 0, [initial, delivery, revised, delivered], { status: 'failed', finished_at: 2500 })
    const view = deriveTimeline(detail([current]), undefined)
    expect(view.timelines.get('r1')!.map((entry) => entry.sequence)).toEqual([1, 2, 3, 4])
    expect(view.metrics.get('r1')!.tps).toBe(8)
    expect(computeTps(attemptTpsInput(revised.payload as Record<string, unknown>))).toBe(10)
    expect(observationEventSummary(delivered).facts).toContainEqual({
      label: m.observation_event_delivery_reason(),
      value: 'connection_closed',
    })
  })
})

describe('deriveTimeline ordering', () => {
  test('orders runs by started_at and events by occurred_at then sequence', () => {
    const later = run('b', 2000)
    const earlier = run('a', 1000, [
      event(2, 'checkpoint', {}, 900),
      event(1, 'checkpoint', {}, 300),
      event(3, 'wire', {}, 300),
    ])
    const view = deriveTimeline(detail([later, earlier], 1000), undefined)
    expect(view.orderedRuns.map((r) => r.id)).toEqual(['a', 'b'])
    expect(view.runIndex.get('a')).toBe(1)
    expect(view.runIndex.get('b')).toBe(2)
    expect(view.timelines.get('a')!.map((e) => e.sequence)).toEqual([1, 3, 2])
  })

  test('holds back runs before the loaded event page until earlier history loads', () => {
    const runs = [run('a', 1000), run('b', 2000), run('c', 3000, [event(5, 'wire')]), run('d', 4000)]
    const bounded = { ...detail(runs), older_events_cursor: 5 }
    const view = deriveTimeline(bounded, undefined)
    expect(view.visibleRuns.map((r) => r.id)).toEqual(['c', 'd'])
    expect(view.runIndex.get('c')).toBe(3)
    expect(deriveTimeline(detail(runs), undefined).visibleRuns.map((r) => r.id)).toEqual(['a', 'b', 'c', 'd'])
    const unloaded = { ...detail([run('a', 1000), run('b', 2000)]), older_events_cursor: 5 }
    expect(deriveTimeline(unloaded, undefined).visibleRuns.map((r) => r.id)).toEqual(['a', 'b'])
  })
})

describe('stream item grouping', () => {
  test('merges consecutive neutral process events and folds same-name handoffs', () => {
    const r = run('a', 0, [
      event(1, 'checkpoint', { stage: 'one' }),
      event(2, 'wire', { direction: 'upstream' }),
      event(3, 'client_tool_handoff', { name: 'search' }),
      event(4, 'client_tool_handoff', { name: 'search' }),
      event(5, 'client_tool_handoff', { name: 'other' }),
      event(6, 'run_finished', { status: 'completed' }),
      event(7, 'client_tool_handoff', { name: 'search' }),
    ])
    const items = deriveTimeline(detail([r]), undefined).streams.get('a')!
    expect(items.map((i) => i.type)).toEqual(['process', 'tools', 'tools', 'event', 'tools'])
    expect(itemEvents(items[0]).map((e) => e.sequence)).toEqual([1, 2])
    expect(itemEvents(items[1]).map((e) => e.sequence)).toEqual([3, 4])
    expect(itemEvents(items[2]).map((e) => e.sequence)).toEqual([5])
    expect(itemKey(items[0])).toBe(1)
    expect(itemKey(items[3])).toBe(6)
  })

  test('process-kind events with a non-neutral tone stay on the main stream', () => {
    const r = run('a', 0, [
      event(1, 'client_visible_content', {
        item: 'text:0',
        block_id: 'partial',
        text: 'partial',
        parts: [{ type: 'text', text: 'partial' }],
        complete: false,
      }),
      event(2, 'checkpoint', {}),
    ])
    const items = deriveTimeline(detail([r]), undefined).streams.get('a')!
    expect(items.map((i) => i.type)).toEqual(['event', 'process'])
  })

  test('handoff without a usable name stays an event', () => {
    const r = run('a', 0, [event(1, 'client_tool_handoff', {}), event(2, 'client_tool_handoff', { name: '  ' })])
    const items = deriveTimeline(detail([r]), undefined).streams.get('a')!
    expect(items.map((i) => i.type)).toEqual(['event', 'event'])
  })
})

describe('gap label', () => {
  const handedOff = () => run('a', 0, [event(1, 'client_tool_handoff', { name: 'lookup' })], { finished_at: 1000 })

  test('below the rapid-continuation window produces no gap', () => {
    const view = deriveTimeline(detail([handedOff(), run('b', 2500)]), undefined)
    expect(view.gapLabel(view.orderedRuns[0], view.orderedRuns[1])).toBeNull()
  })

  test('a gap after a tool handoff names the tool', () => {
    const view = deriveTimeline(detail([handedOff(), run('b', 4000)]), undefined)
    expect(view.gapLabel(view.orderedRuns[0], view.orderedRuns[1])).toBe(
      m.observation_gap_tool({ tool: 'lookup', duration: formatDuration(3000) }),
    )
  })

  test('a gap without a handoff reads as idle', () => {
    const idle = run('a', 0, [], { finished_at: 1000 })
    const view = deriveTimeline(detail([idle, run('b', 4000)]), undefined)
    expect(view.gapLabel(view.orderedRuns[0], view.orderedRuns[1])).toBe(
      m.observation_gap_idle({ duration: formatDuration(3000) }),
    )
  })

  test('late terminal revisions leave the client tool gap anchored at delivery', () => {
    const previous = run(
      'a',
      0,
      [
        event(1, 'client_tool_handoff', { name: 'lookup' }, 1000),
        event(2, 'run_finished', { delivery_completed_at: 1000 }, 7000),
      ],
      { delivery_completed_at: 1000, finished_at: 7000 },
    )
    const view = deriveTimeline(detail([previous, run('b', 4000)]), undefined)
    expect(view.metrics.get('a')!.durationMs).toBe(1000)
    expect(view.gapLabel(view.orderedRuns[0], view.orderedRuns[1])).toBe(
      m.observation_gap_tool({ tool: 'lookup', duration: formatDuration(3000) }),
    )
  })

  test('an unfinished run falls back to its last event time', () => {
    const unfinished = run('a', 0, [event(1, 'checkpoint', {}, 500)])
    const view = deriveTimeline(detail([unfinished, run('b', 4000)]), undefined)
    expect(view.gapLabel(view.orderedRuns[0], view.orderedRuns[1])).toBe(
      m.observation_gap_idle({ duration: formatDuration(3500) }),
    )
  })
})

describe('offset label', () => {
  test('offsets relative to the interaction start carry a sign', () => {
    const view = deriveTimeline(detail([], 10_000), undefined)
    expect(view.offsetLabel(11_500)).toBe(`+${formatDuration(1500)}`)
    expect(view.offsetLabel(9_000)).toBe(`−${formatDuration(1000)}`)
  })

  test('without interaction or failure the offset falls back to clock time', () => {
    const view = deriveTimeline(undefined, undefined)
    expect(view.title).toBe(m.observation_details())
    expect(view.offsetLabel(0)).toBe(formatTime(0))
  })
})

describe('title', () => {
  test('prefers the last run model name, then route, then interaction fields', () => {
    const named = run('a', 0, [], { model_display_name: 'Named Model' })
    expect(deriveTimeline(detail([named]), undefined).title).toBe('Named Model')

    const blank = run('a', 0, [], { model_display_name: '  ', route_id: 'route-x' })
    expect(deriveTimeline(detail([blank]), undefined).title).toBe('route-x')

    const bare = detail([])
    bare.interaction.first_model_display_name = 'First'
    expect(deriveTimeline(bare, undefined).title).toBe('First')
    bare.interaction.first_model_display_name = ' '
    expect(deriveTimeline(bare, undefined).title).toBe('first-route')
  })

  test('a failure detail titles as a failed request', () => {
    expect(deriveTimeline(undefined, failureDetail([])).title).toBe(m.observation_request_failed())
  })
})

describe('failure items', () => {
  test('failure events flatten through the same ordering and grouping', () => {
    const failure = failureDetail([
      event(2, 'checkpoint', {}, 60),
      event(1, 'client_tool_handoff', { name: 'x' }, 50),
      event(3, 'checkpoint', {}, 70),
    ])
    const view = deriveTimeline(undefined, failure)
    expect(view.failureItems.map((i) => i.type)).toEqual(['tools', 'process'])
    expect(view.failureItems.map(itemKey)).toEqual([1, 2])
    expect(itemEvents(view.failureItems[1]).map((e) => e.sequence)).toEqual([2, 3])
  })
})

describe('run metrics', () => {
  const started = (sequence: number, attempt: string, model: string, provider = 'Service') =>
    event(sequence, 'target_attempt_started', { attempt_id: attempt, upstream_model: model, provider_name: provider })
  const finished = (sequence: number, attempt: string, status: string, extra: Record<string, unknown> = {}) =>
    event(sequence, 'target_attempt_finished', { attempt_id: attempt, status, ...extra })

  test('a failed attempt before fallback does not describe the serving upstream', () => {
    const metrics = runMetrics(
      run(
        'r1',
        0,
        [
          started(1, 'a1', 'primary-model', 'Primary'),
          finished(2, 'a1', 'failed', {
            duration_ms: 1000,
            first_token_ms: 300,
            usage: { ...usage, output_tokens: 5 },
          }),
          started(3, 'a2', 'fallback-model', 'Fallback'),
          finished(4, 'a2', 'completed', {
            duration_ms: 3000,
            first_token_ms: 1000,
            usage: { ...usage, output_tokens: 100 },
          }),
        ],
        { delivery_completed_at: 5000, usage: { ...usage, output_tokens: 105 } },
      ),
    )
    expect(metrics).toEqual({
      upstream: [{ model: 'fallback-model', provider: 'Fallback' }],
      durationMs: 5000,
      firstTokenMs: 1000,
      tps: 21,
    })
  })

  test('an attempt still in progress replaces the failed one before it', () => {
    const metrics = runMetrics(
      run(
        'r1',
        0,
        [
          started(1, 'a1', 'primary-model'),
          finished(2, 'a1', 'failed', { duration_ms: 400, first_token_ms: 300, usage: { ...usage, output_tokens: 5 } }),
          started(3, 'a2', 'retry-model'),
        ],
        { status: 'running' },
      ),
    )
    expect(metrics).toEqual({
      upstream: [{ model: 'retry-model', provider: 'Service' }],
      durationMs: null,
      firstTokenMs: null,
      tps: null,
    })
  })

  test('several model turns include run overhead and platform tool time', () => {
    const metrics = runMetrics(
      run(
        'r1',
        0,
        [
          started(1, 'a1', 'shared-model'),
          finished(2, 'a1', 'completed', {
            duration_ms: 2000,
            first_token_ms: 1000,
            usage: { ...usage, output_tokens: 10 },
          }),
          started(3, 'a2', 'shared-model'),
          finished(4, 'a2', 'completed', {
            duration_ms: 4000,
            first_token_ms: 500,
            usage: { ...usage, output_tokens: 80 },
          }),
        ],
        { usage: { ...usage, output_tokens: 90 }, delivery_completed_at: 9000, finished_at: 8000 },
      ),
    )
    expect(metrics).toEqual({
      upstream: [{ model: 'shared-model', provider: 'Service' }],
      durationMs: 9000,
      firstTokenMs: 1000,
      tps: 10,
    })
  })

  test('a run without attempts reports nothing instead of zeros', () => {
    expect(runMetrics(run('r1', 0, [event(1, 'run_admitted', { route_id: 'route' })]))).toEqual({
      upstream: [],
      durationMs: null,
      firstTokenMs: null,
      tps: null,
    })
  })

  test('R1 uses delivery lifetime independently of attempt speed and first token', () => {
    const attempt = finished(2, 'a1', 'completed', {
      duration_ms: 13857,
      first_token_ms: 11918,
      usage: { ...usage, output_tokens: 896 },
    })
    const metrics = runMetrics(
      run('r1', 0, [started(1, 'a1', 'synthetic'), attempt], {
        delivery_completed_at: 13915,
        usage: { ...usage, output_tokens: 896 },
      }),
    )
    expect(metrics.tps).toBeCloseTo(64.3909, 4)
    expect(metrics.durationMs).toBe(13915)
    expect(metrics.firstTokenMs).toBe(11918)
    expect(computeTps(attemptTpsInput(attempt.payload as Record<string, unknown>))).toBeCloseTo(64.6605, 4)
  })

  test('streaming and nonstreaming attempts use equal full duration throughput', () => {
    const payload = { duration_ms: 1000, usage: { ...usage, output_tokens: 5 } }
    for (const first of [null, 0, 950, 951, 1000]) {
      expect(computeTps(attemptTpsInput({ ...payload, first_token_ms: first }))).toBe(5)
    }
    expect(computeTps(attemptTpsInput({ usage }))).toBeNull()
    expect(computeTps(attemptTpsInput({ ...payload, usage: { ...usage, output_tokens: 0 } }))).toBe(0)
    expect(computeTps(attemptTpsInput({ ...payload, usage: { ...usage, output_tokens: null } }))).toBeNull()
  })

  test.each([
    [1000, 950, 5],
    [1000, 951, 5],
    [1000, null, 5],
    [1000, 0, 5],
    [0, 0, null],
  ])('full duration boundary for duration=%s and first token=%s', (duration, first, tps) => {
    expect(
      runMetrics(
        run(
          'r1',
          0,
          [
            started(1, 'a1', 'synthetic'),
            finished(2, 'a1', 'completed', {
              duration_ms: duration,
              first_token_ms: first,
              usage: { ...usage, output_tokens: 5 },
            }),
          ],
          { delivery_completed_at: duration, usage: { ...usage, output_tokens: 5 } },
        ),
      ).tps,
    ).toBe(tps)
  })

  test('late usage revisions do not extend delivery duration or double count output', () => {
    const current = run(
      'r1',
      1000,
      [
        finished(1, 'a1', 'completed', { duration_ms: 1000, usage: { ...usage, output_tokens: 10 } }),
        event(2, 'run_finished', { delivery_completed_at: 3000 }, 3000),
        finished(3, 'a1', 'completed', { duration_ms: 1000, usage: { ...usage, output_tokens: 20 } }),
        event(4, 'run_finished', { delivery_completed_at: 3000 }, 9000),
      ],
      { delivery_completed_at: 3000, finished_at: 9000, usage: { ...usage } },
    )
    expect(runMetrics(current).tps).toBe(10)
    expect(runMetrics(current).durationMs).toBe(2000)
    current.usage.output_tokens = 0
    expect(runMetrics(current).tps).toBe(0)
    current.usage.output_tokens = null
    expect(runMetrics(current).tps).toBeNull()
  })

  test('partial output coverage makes run speed unknown', () => {
    const current = run('r1', 0, [], {
      delivery_completed_at: 1000,
      usage: {
        ...usage,
        coverage: {
          attempt_count: 2,
          missing_output_tokens: 1,
          missing_input_tokens: 0,
          missing_cache_read_tokens: 0,
          missing_cache_write_tokens: 0,
          missing_reasoning_tokens: 0,
        },
      },
    })
    expect(runMetrics(current).tps).toBeNull()
  })

  test('successful runs with unknown delivery duration stay unknown even with terminal events', () => {
    const current = run('r1', 0, [event(1, 'run_finished', { delivery_completed_at: 2000 })], { finished_at: 2000 })
    expect(runMetrics(current).tps).toBeNull()
    expect(runMetrics(current).durationMs).toBeNull()
  })

  test.each(['failed', 'cancelled', 'interrupted', 'disconnected', 'user_interrupted'])(
    'unsuccessful %s runs use recorded terminal time',
    (status) => {
      expect(runMetrics(run('r1', 1000, [], { status, finished_at: 3000 })).tps).toBe(10)
    },
  )

  test('paginated events and inter-request client tool gaps do not alter run speed', () => {
    const first = run('r1', 0, [], { delivery_completed_at: 1000 })
    const second = run('r2', 100_000, [], { delivery_completed_at: 101_000 })
    const view = deriveTimeline(detail([first, second]), undefined)
    expect(view.metrics.get('r1')!.tps).toBe(20)
    expect(view.metrics.get('r2')!.tps).toBe(20)
  })
})
