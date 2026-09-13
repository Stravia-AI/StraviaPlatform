import { describe, expect, test } from 'bun:test'
import { observationConversationMessages } from '../src/lib/observation-conversation'
import { mergeObservationRuns, retainLiveBlocks, withoutCommittedBlocks } from '../src/lib/observation-state'
import type { InteractionDetail, LiveContentBlock, ObservationEvent, RunDetail } from '../src/lib/types/observation'

const usage = {
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_write_tokens: null,
  reasoning_tokens: null,
}
function event(runId: string, sequence: number, kind: string, payload: unknown): ObservationEvent {
  return {
    sequence,
    occurred_at: sequence,
    interaction_id: 'interaction',
    run_id: runId,
    rejection_id: null,
    kind,
    payload,
  }
}
function run(id: string, startedAt: number, events: ObservationEvent[]): RunDetail {
  return {
    id,
    parent_run_id: null,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: 'model',
    model_display_name: 'Model',
    ingress_protocol: 'openai',
    status: 'completed',
    terminal_reason: 'completed',
    user_interrupted: false,
    debug_enabled: true,
    client_output_committed: true,
    started_at: startedAt,
    finished_at: startedAt + 1,
    usage,
    events,
    trace: null,
  }
}
function detail(runs: RunDetail[], tail = ''): InteractionDetail {
  const interaction = {
    id: 'interaction',
    root_id: 'root',
    parent_interaction_id: null,
    generation_root_id: null,
    first_route_id: 'model',
    first_model_display_name: 'Model',
    status: 'completed',
    started_at: 0,
    last_active_at: 10,
    input_preview: 'Actual user question',
    visible_tail: tail,
    usage,
    debug_status: 'complete',
    observation_gap: false,
    matched: true,
    last_event_sequence: 10,
    context_events: [],
  }
  return {
    interaction,
    root: { id: 'root', last_active_at: 10, interactions: [interaction] },
    runs,
    snapshot_sequence: 10,
    older_events_cursor: null,
  }
}

describe('observation conversation', () => {
  test('prepending and replaying overlapping event pages preserves causal text and existing messages', () => {
    const first = run('first', 1, [event('first', 2, 'client_visible_content_delta', { text: 'Unchanged' })])
    const latest = run('latest', 2, [event('latest', 5, 'client_visible_content_delta', { text: 'B' })])
    const current = detail([first, latest])
    const before = observationConversationMessages(current)
    const older = { ...latest, events: [event('latest', 4, 'client_visible_content_delta', { text: 'A' }), latest.events[0]] }
    const merged = mergeObservationRuns(current.runs, [older], true)
    const replay = mergeObservationRuns(merged, [older], true)
    const after = observationConversationMessages({ ...current, runs: replay }, [], before)
    expect(after.map((message) => message.text)).toEqual(['Actual user question', 'Unchanged', 'AB'])
    expect(after[1]).toBe(before[1])
    expect(replay[0]).toBe(first)
    expect(replay[1]).toBe(merged[1])
  })

  test('live blocks extend one Markdown message and commit removes only the matching overlay', () => {
    const current = detail([run('first', 1, [event('first', 1, 'client_visible_content_delta', { text: '| A | B |\\n' })])])
    const block: LiveContentBlock = { block_id: 'block-a', interaction_id: 'interaction', run_id: 'first', kind: 'client_visible_content_delta', model_turn_id: 'turn', attempt_id: 'attempt', occurred_at: 2, revision: 1, text: '|---|---|\\n| one | two |' }
    const live = observationConversationMessages(current, [block])
    expect(live[1].text).toBe('| A | B |\\n|---|---|\\n| one | two |')
    expect(live[1].unsaved).toBe(true)
    const committed = { ...current, runs: mergeObservationRuns(current.runs, [{ ...current.runs[0], events: [event('first', 2, block.kind, { text: block.text, block_id: block.block_id })] }]) }
    const after = observationConversationMessages(committed, withoutCommittedBlocks([block], committed), live)
    expect(after[1].text).toBe(live[1].text)
    expect(after[1].unsaved).toBe(false)
    expect(observationConversationMessages(current, [])[1].text).toBe('| A | B |\\n')
  })

  test('a bounded latest page never substitutes the full interaction tail', () => {
    const current = { ...detail([run('latest', 2, [])], 'Unloaded historical output'), older_events_cursor: 50 }
    expect(observationConversationMessages(current)[1].text).toBe('')
  })

  test('background live memory is bounded independently from the selected interaction', () => {
    const blocks: LiveContentBlock[] = Array.from({ length: 100 }, (_, index) => ({ block_id: String(index), interaction_id: index === 0 ? 'selected' : 'background', run_id: String(index), kind: 'client_visible_content_delta', model_turn_id: null, attempt_id: null, occurred_at: index, revision: 1, text: 'x'.repeat(16_384) }))
    const retained = retainLiveBlocks(blocks, 'selected')
    expect(retained.find((block) => block.interaction_id === 'selected')).toBe(blocks[0])
    expect(retained.filter((block) => block.interaction_id !== 'selected').reduce((bytes, block) => bytes + block.text.length * 2, 0)).toBeLessThanOrEqual(256 * 1024)
  })
  test('shows the user once and orders delivered text without exposing tool or checkpoint payloads', () => {
    const messages = observationConversationMessages(
      detail(
        [
          run('child', 2, [event('child', 8, 'client_visible_content_delta', { text: 'Second response' })]),
          run('first', 1, [
            event('first', 4, 'client_visible_content_delta', { text: ' world' }),
            event('first', 3, 'checkpoint', { text: 'private system prompt' }),
            event('first', 2, 'client_visible_content_delta', { text: 'Hello' }),
            event('first', 5, 'platform_tool_finished', { text: 'private tool result' }),
            event('other-run', 6, 'client_visible_content_delta', { text: 'unrelated response' }),
            event('first', 7, 'client_visible_content_delta', { text: { raw: 'not display text' } }),
          ]),
        ],
        'Second response',
      ),
    )
    expect(messages.map(({ role, text }) => ({ role, text }))).toEqual([
      { role: 'user', text: 'Actual user question' },
      { role: 'assistant', text: 'Hello world' },
      { role: 'assistant', text: 'Second response' },
    ])
  })

  test('uses the retained tail only once when no run has delivered text events', () => {
    const messages = observationConversationMessages(
      detail([run('first', 1, []), run('child', 2, [])], 'Retained ending'),
    )
    expect(messages.filter((message) => message.role === 'assistant').map((message) => message.text)).toEqual([
      '',
      'Retained ending',
    ])
    const current = observationConversationMessages(
      detail(
        [
          run('first', 1, [event('first', 2, 'client_visible_content_delta', { text: 'Actual response' })]),
          run('child', 2, []),
        ],
        'Retained ending',
      ),
    )
    expect(current.filter((message) => message.role === 'assistant').map((message) => message.text)).toEqual([
      'Actual response',
      '',
    ])
  })

  test('keeps missing historical input empty while preserving the retained output', () => {
    const historical = detail([], 'Only the preserved ending')
    historical.interaction.input_preview = null
    const messages = observationConversationMessages(historical)
    expect(messages.map(({ role, text }) => ({ role, text }))).toEqual([
      { role: 'user', text: '' },
      { role: 'assistant', text: 'Only the preserved ending' },
    ])
  })
})
