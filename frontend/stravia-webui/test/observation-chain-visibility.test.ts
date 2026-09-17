import { describe, expect, test } from 'bun:test'
import { hiddenFailureNode, interactionDisplayStatus } from '../src/lib/observation-chain-visibility'
import type { InteractionSummary } from '../src/lib/types/observation'

const usage = {
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_write_tokens: null,
  reasoning_tokens: null,
}

function interaction(overrides: Partial<InteractionSummary>): InteractionSummary {
  return {
    id: 'node',
    root_id: 'root',
    parent_interaction_id: null,
    generation_root_id: 'root',
    first_route_id: 'model',
    first_model_display_name: 'Model',
    status: 'interrupted',
    started_at: 0,
    last_active_at: 1,
    input_preview: 'input',
    visible_tail: '',
    failed_request: false,
    client_output_delivered: false,
    usage,
    debug_status: 'none',
    observation_gap: false,
    matched: true,
    last_event_sequence: 1,
    context_events: [],
    ...overrides,
  }
}

describe('hiddenFailureNode', () => {
  test('hides terminal failure without any delivered client output', () => {
    expect(hiddenFailureNode(interaction({ failed_request: true }))).toBe(true)
  })

  test('keeps nodes that delivered any client-visible byte', () => {
    expect(
      hiddenFailureNode(interaction({ failed_request: true, client_output_delivered: true })),
    ).toBe(false)
  })

  test('keeps live interactions even when a run already failed', () => {
    expect(hiddenFailureNode(interaction({ failed_request: true, status: 'running' }))).toBe(false)
    expect(
      hiddenFailureNode(interaction({ failed_request: true, status: 'waiting_client' })),
    ).toBe(false)
  })

  test('keeps cancels, disconnects, and observation gaps that never failed', () => {
    expect(hiddenFailureNode(interaction({ status: 'interrupted' }))).toBe(false)
    expect(hiddenFailureNode(interaction({ status: 'disconnected' }))).toBe(false)
    expect(hiddenFailureNode(interaction({ status: 'user_interrupted' }))).toBe(false)
  })
})

describe('interactionDisplayStatus', () => {
  test('surfaces terminal failure over the interrupted rollup', () => {
    expect(interactionDisplayStatus(interaction({ failed_request: true }))).toBe('failed')
  })

  test('keeps process statuses while the interaction is still advancing', () => {
    expect(interactionDisplayStatus(interaction({ failed_request: true, status: 'running' }))).toBe(
      'running',
    )
    expect(
      interactionDisplayStatus(interaction({ failed_request: true, status: 'waiting_client' })),
    ).toBe('waiting_client')
  })

  test('leaves non-failing statuses untouched', () => {
    expect(interactionDisplayStatus(interaction({ status: 'completed' }))).toBe('completed')
    expect(interactionDisplayStatus(interaction({ status: 'user_interrupted' }))).toBe(
      'user_interrupted',
    )
  })
})
