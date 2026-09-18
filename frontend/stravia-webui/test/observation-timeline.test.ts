import { describe, expect, test } from 'bun:test'
import { formatDuration, formatList, formatTime } from '../src/lib/format'
import { deriveTimeline, itemEvents, itemKey, processGroup, usageText } from '../src/lib/observation-timeline'
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

function run(id: string, started_at: number, events: ObservationEvent[] = [], extra: Partial<RunDetail> = {}): RunDetail {
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
      error: { source: 'upstream', code: 'upstream_timeout', message: 'boom', status_code: 504 },
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
})

describe('stream item grouping', () => {
  test('merges consecutive neutral process events and folds same-name handoffs', () => {
    const r = run('a', 0, [
      event(1, 'checkpoint', { stage: 'one' }),
      event(2, 'usage_confirmed', { usage: {} }),
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
      event(1, 'trace_manifest_updated', { status: 'missing' }),
      event(2, 'checkpoint', {}),
    ])
    const items = deriveTimeline(detail([r]), undefined).streams.get('a')!
    expect(items.map((i) => i.type)).toEqual(['event', 'process'])
  })

  test('handoff without a usable name stays an event', () => {
    const r = run('a', 0, [
      event(1, 'client_tool_handoff', {}),
      event(2, 'client_tool_handoff', { name: '  ' }),
    ])
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

describe('processGroup', () => {
  test('a single kind collapses into title times count', () => {
    const group = processGroup([
      event(1, 'checkpoint', { stage: 'a' }),
      event(2, 'checkpoint', { stage: 'b' }),
    ])
    expect(group.label).toBe(m.observation_event_group({ title: m.observation_event_checkpoint(), count: 2 }))
    expect(group.titles).toBeNull()
  })

  test('mixed kinds list distinct titles and truncate past three', () => {
    const kinds = ['checkpoint', 'wire', 'usage_confirmed']
    const mixed = kinds.map((kind, i) => event(i + 1, kind, {}))
    const three = processGroup(mixed)
    expect(three.label).toBe(m.observation_process_events({ count: 3 }))
    expect(three.titles).toBe(
      formatList([m.observation_event_checkpoint(), m.observation_event_wire(), m.observation_event_usage_confirmed()]),
    )

    const four = processGroup([...mixed, event(4, 'input_preview_recorded', {})])
    expect(four.label).toBe(m.observation_process_events({ count: 4 }))
    expect(four.titles).toBe(
      `${formatList([m.observation_event_checkpoint(), m.observation_event_wire(), m.observation_event_usage_confirmed()])}…`,
    )
  })
})

describe('usageText', () => {
  test('renders unknown for null and numbers otherwise', () => {
    expect(usageText(null)).toBe(m.observation_usage_unknown())
    expect(usageText(12)).toBe('12')
  })
})
