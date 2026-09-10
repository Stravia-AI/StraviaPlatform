import { describe, expect, test } from 'bun:test'
import { observationConversationMessages } from '../src/lib/observation-conversation'
import type { InteractionDetail, ObservationEvent, RunDetail } from '../src/lib/types/observation'

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
    debug_events: [{ content: 'private Debug payload' }],
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
  }
}

describe('observation conversation', () => {
  test('shows the user once and orders delivered text without exposing tool or Debug payloads', () => {
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
