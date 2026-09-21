import { describe, expect, test } from 'bun:test'
import {
  hasHistoricalFailure,
  hiddenFailureNode,
  interactionDisplayStatus,
} from '../src/lib/observation-chain-visibility'
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

  test('keeps completed nodes with failed history once client output was delivered', () => {
    expect(
      hiddenFailureNode(interaction({ status: 'completed', failed_request: true, client_output_delivered: true })),
    ).toBe(false)
  })

  test('keeps the existing hidden rule for an anomalous completed failure with no delivered output', () => {
    expect(
      hiddenFailureNode(interaction({ status: 'completed', failed_request: true, client_output_delivered: false })),
    ).toBe(true)
  })

  test('keeps live interactions even when a run already failed', () => {
    expect(hiddenFailureNode(interaction({ failed_request: true, status: 'running' }))).toBe(false)
    expect(hiddenFailureNode(interaction({ failed_request: true, status: 'waiting_client' }))).toBe(false)
  })

  test('keeps cancels, disconnects, and observation gaps that never failed', () => {
    expect(hiddenFailureNode(interaction({ status: 'interrupted' }))).toBe(false)
    expect(hiddenFailureNode(interaction({ status: 'disconnected' }))).toBe(false)
    expect(hiddenFailureNode(interaction({ status: 'user_interrupted' }))).toBe(false)
  })
})

describe('hasHistoricalFailure', () => {
  test('marks failed history only when it is not already the primary failure state', () => {
    expect(hasHistoricalFailure(interaction({ status: 'completed', failed_request: true }))).toBe(true)
    expect(hasHistoricalFailure(interaction({ status: 'running', failed_request: true }))).toBe(true)
    expect(hasHistoricalFailure(interaction({ status: 'waiting_client', failed_request: true }))).toBe(true)
    expect(hasHistoricalFailure(interaction({ status: 'interrupted', failed_request: true }))).toBe(false)
    expect(hasHistoricalFailure(interaction({ status: 'completed', failed_request: false }))).toBe(false)
  })
})

describe('interactionDisplayStatus', () => {
  test('keeps a completed interaction primary even when an earlier request failed', () => {
    expect(interactionDisplayStatus(interaction({ status: 'completed', failed_request: true }))).toBe('completed')
  })

  test('surfaces a failed interrupted interaction as failed', () => {
    expect(interactionDisplayStatus(interaction({ status: 'interrupted', failed_request: true }))).toBe('failed')
  })

  test('keeps process statuses while the interaction is still advancing', () => {
    expect(interactionDisplayStatus(interaction({ failed_request: true, status: 'running' }))).toBe('running')
    expect(interactionDisplayStatus(interaction({ failed_request: true, status: 'waiting_client' }))).toBe(
      'waiting_client',
    )
  })

  test('leaves statuses without a failed request untouched', () => {
    expect(interactionDisplayStatus(interaction({ status: 'completed' }))).toBe('completed')
    expect(interactionDisplayStatus(interaction({ status: 'user_interrupted' }))).toBe('user_interrupted')
  })
})
