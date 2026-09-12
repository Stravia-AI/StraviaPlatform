import { describe, expect, test } from 'bun:test'
import { canvasLinks, visualParent } from '../src/lib/interaction-canvas-links'
import type { InteractionSummary, ObservationEvent } from '../src/lib/types/observation'

const usage = {
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_write_tokens: null,
  reasoning_tokens: null,
}

function event(kind: string, payload: Record<string, unknown>): ObservationEvent {
  return { sequence: 1, occurred_at: 1, interaction_id: null, run_id: null, rejection_id: null, kind, payload }
}

function interaction(
  id: string,
  parent: string | null,
  startedAt: number,
  context: ObservationEvent[] = [],
): InteractionSummary {
  return {
    id,
    root_id: parent ? 'root-a' : id,
    parent_interaction_id: parent,
    generation_root_id: parent ? 'root-a' : id,
    first_route_id: id,
    first_model_display_name: id,
    status: 'completed',
    started_at: startedAt,
    last_active_at: startedAt + 1,
    input_preview: id,
    visible_tail: id,
    usage,
    debug_status: 'none',
    observation_gap: false,
    matched: true,
    last_event_sequence: 1,
    context_events: context,
  }
}

function inferred(source: string): ObservationEvent {
  return event('retained_tail_associated', { source_interaction_id: source, status: 'inferred' })
}

function tail(status: string, source?: string): ObservationEvent {
  return event('retained_tail_associated', { status, ...(source ? { source_interaction_id: source } : {}) })
}

describe('canvas links', () => {
  test('keeps two confirmed children of the same parent as a fork', () => {
    const atlas = interaction('atlas', null, 0)
    const boreal = interaction('boreal', 'atlas', 60_000)
    const cinder = interaction('cinder', 'atlas', 120_000)
    expect(canvasLinks([atlas, boreal, cinder])).toEqual([
      { id: 'confirmed-atlas-boreal', source: 'atlas', target: 'boreal', kind: 'confirmed' },
      { id: 'confirmed-atlas-cinder', source: 'atlas', target: 'cinder', kind: 'confirmed' },
    ])
  })

  test('wires a model-switch continuation as a downward spine instead of a sibling fork', () => {
    const first = interaction('glm-first', null, 1_789_199_314_993)
    const switched = interaction('gpt-switch', null, 1_789_199_329_107, [inferred('glm-first')])
    const resumed = interaction('glm-resume', 'glm-first', 1_789_199_345_390)
    const nodes = [first, switched, resumed]
    expect(visualParent(switched, nodes)?.id).toBe('glm-first')
    expect(visualParent(resumed, nodes)?.id).toBe('gpt-switch')
    expect(canvasLinks(nodes)).toEqual([
      { id: 'inferred-glm-first-gpt-switch', source: 'glm-first', target: 'gpt-switch', kind: 'inferred' },
      { id: 'inferred-gpt-switch-glm-resume', source: 'gpt-switch', target: 'glm-resume', kind: 'inferred' },
    ])
  })

  test('does not draw a skipped generation parent beside the inferred continuation', () => {
    const first = interaction('glm-first', null, 10)
    const switched = interaction('gpt-switch', null, 20, [inferred('glm-first')])
    const resumed = interaction('glm-resume', 'glm-first', 30)
    const sources = canvasLinks([first, switched, resumed]).map((link) => `${link.source}->${link.target}`)
    expect(sources).toEqual(['glm-first->gpt-switch', 'gpt-switch->glm-resume'])
    expect(sources).not.toContain('glm-first->glm-resume')
  })

  test('keeps a confirmed chain when there is no inferred intermediate', () => {
    const root = interaction('root', null, 0)
    const child = interaction('child', 'root', 10)
    const grandchild = interaction('grandchild', 'child', 20)
    expect(canvasLinks([root, child, grandchild])).toEqual([
      { id: 'confirmed-root-child', source: 'root', target: 'child', kind: 'confirmed' },
      { id: 'confirmed-child-grandchild', source: 'child', target: 'grandchild', kind: 'confirmed' },
    ])
  })

  test('keeps a real fork when the resume omitted the switched model turn', () => {
    const first = interaction('glm-first', null, 10)
    const switched = interaction('gpt-switch', null, 20, [inferred('glm-first')])
    const forked = interaction('glm-fork', 'glm-first', 30, [inferred('glm-first')])
    expect(canvasLinks([first, switched, forked])).toEqual([
      { id: 'inferred-glm-first-gpt-switch', source: 'glm-first', target: 'gpt-switch', kind: 'inferred' },
      { id: 'confirmed-glm-first-glm-fork', source: 'glm-first', target: 'glm-fork', kind: 'confirmed' },
    ])
  })

  test('wires a resume that retained the switched turn under that intermediate', () => {
    const first = interaction('glm-first', null, 10)
    const switched = interaction('gpt-switch', null, 20, [inferred('glm-first')])
    const resumed = interaction('glm-resume', 'glm-first', 30, [inferred('gpt-switch')])
    expect(canvasLinks([first, switched, resumed])).toEqual([
      { id: 'inferred-glm-first-gpt-switch', source: 'glm-first', target: 'gpt-switch', kind: 'inferred' },
      { id: 'inferred-gpt-switch-glm-resume', source: 'gpt-switch', target: 'glm-resume', kind: 'inferred' },
    ])
  })

  test('keeps a fork when retained tail ran and did not match the switched turn', () => {
    const first = interaction('glm-first', null, 10)
    const switched = interaction('gpt-switch', null, 20, [inferred('glm-first')])
    const forked = interaction('glm-fork', 'glm-first', 30, [tail('no_match')])
    const sources = canvasLinks([first, switched, forked]).map((link) => `${link.source}->${link.target}`)
    expect(sources).toEqual(['glm-first->gpt-switch', 'glm-first->glm-fork'])
  })

  test('keeps a later same-model turn under the linearized resume node', () => {
    const first = interaction('glm-first', null, 10)
    const switched = interaction('gpt-switch', null, 20, [inferred('glm-first')])
    const resumed = interaction('glm-resume', 'glm-first', 30)
    const next = interaction('glm-next', 'glm-resume', 40)
    expect(canvasLinks([first, switched, resumed, next])).toEqual([
      { id: 'inferred-glm-first-gpt-switch', source: 'glm-first', target: 'gpt-switch', kind: 'inferred' },
      { id: 'inferred-gpt-switch-glm-resume', source: 'gpt-switch', target: 'glm-resume', kind: 'inferred' },
      { id: 'confirmed-glm-resume-glm-next', source: 'glm-resume', target: 'glm-next', kind: 'confirmed' },
    ])
  })

  test('replaces the confirmed parent with a native compaction source on the same node', () => {
    const root = interaction('root', null, 0)
    const child = interaction('child', 'root', 10, [
      event('native_compaction_associated', { source_interaction_id: 'root' }),
    ])
    expect(canvasLinks([root, child])).toEqual([
      { id: 'native-root-child', source: 'root', target: 'child', kind: 'native' },
    ])
  })
})
