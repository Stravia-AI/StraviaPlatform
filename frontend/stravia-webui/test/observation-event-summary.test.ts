import { describe, expect, test } from 'bun:test'
import { observationEventSummary } from '../src/lib/observation-event-summary'
import * as m from '../src/lib/paraglide/messages.js'
import { getLocale, overwriteGetLocale } from '../src/lib/paraglide/runtime.js'
import type { ObservationEvent } from '../src/lib/types'

function event(sequence: number, kind: string, payload: unknown): ObservationEvent {
  return {
    sequence,
    kind,
    payload,
    occurred_at: sequence,
    interaction_id: 'interaction',
    run_id: 'run',
    rejection_id: null,
  }
}

function speed(completion: ObservationEvent): string | undefined {
  return observationEventSummary(completion).facts.find((fact) => fact.label === m.logs_token_speed())?.value
}

describe('observation attempt output speed', () => {
  test('uses the usage carried by the finish event itself', () => {
    expect(
      speed(
        event(10, 'target_attempt_finished', {
          attempt_id: 'current',
          status: 'completed',
          duration_ms: 18_750,
          first_token_ms: 7_650,
          usage: { output_tokens: 1110 },
        }),
      ),
    ).toBe('100 tok/s')
  })

  test('does not invent throughput when usage is absent or its output is unknown', () => {
    expect(
      speed(
        event(10, 'target_attempt_finished', {
          attempt_id: 'current',
          status: 'completed',
          duration_ms: 18_750,
          first_token_ms: 7_650,
        }),
      ),
    ).toBe('–')
    expect(
      speed(
        event(10, 'target_attempt_finished', {
          attempt_id: 'current',
          status: 'completed',
          duration_ms: 18_750,
          first_token_ms: 7_650,
          usage: { output_tokens: null },
        }),
      ),
    ).toBe('–')
  })
})

describe('observation usage on attempt finish', () => {
  test('keeps cache counters separate without exposing reasoning as another output counter', () => {
    const summary = observationEventSummary(
      event(5, 'target_attempt_finished', {
        attempt_id: 'current',
        status: 'completed',
        duration_ms: 1_000,
        usage: {
          input_tokens: 920,
          output_tokens: 86,
          cache_read_tokens: 320,
          cache_write_tokens: 12,
          reasoning_tokens: 4_444,
        },
      }),
    )

    expect(summary.facts).toContainEqual({ label: m.observation_event_tokens_input(), value: '920' })
    expect(summary.facts).toContainEqual({ label: m.observation_event_tokens_output(), value: '86' })
    expect(summary.facts).toContainEqual({ label: m.observation_event_tokens_cache_read(), value: '320' })
    expect(summary.facts).toContainEqual({ label: m.observation_event_tokens_cache_write(), value: '12' })
    expect(summary.facts.some((fact) => fact.value === '4,444')).toBe(false)
  })
})

describe('observation merged lifecycle payloads', () => {
  test('run_finished renders the merged delivery status and reason with distinct labels', () => {
    const summary = observationEventSummary(
      event(9, 'run_finished', {
        status: 'completed',
        terminal_reason: 'completed',
        delivery: { status: 'delivered', reason: null },
      }),
    )
    expect(summary.title).toBe(m.observation_event_run_finished())
    expect(summary.facts).toContainEqual({ label: m.observation_delivery(), value: m.observation_event_delivered() })
    expect(summary.facts).toContainEqual({ label: m.observation_event_reason(), value: 'completed' })
  })

  test('a failed delivery is readable without erasing the run status', () => {
    const summary = observationEventSummary(
      event(9, 'run_finished', {
        status: 'failed',
        terminal_reason: 'upstream_timeout',
        delivery: { status: 'delivery_failed', reason: 'websocket_delivery_dropped' },
      }),
    )
    expect(summary.tone).toBe('error')
    expect(summary.facts).toContainEqual({ label: m.observation_delivery(), value: m.observation_status_failed() })
    expect(summary.facts).toContainEqual({
      label: m.observation_event_delivery_reason(),
      value: 'websocket_delivery_dropped',
    })
  })

  test('a cancelled delivery keeps the run result readable', () => {
    const summary = observationEventSummary(
      event(9, 'run_finished', { status: 'cancelled', delivery: { status: 'cancelled', reason: 'user_cancelled' } }),
    )
    expect(summary.facts).toContainEqual({ label: m.observation_delivery(), value: m.observation_status_cancelled() })
    expect(summary.facts).toContainEqual({ label: m.observation_event_delivery_reason(), value: 'user_cancelled' })
  })

  test('run_state_changed explains a restart recovery instead of the raw reason', () => {
    const summary = observationEventSummary(
      event(9, 'run_state_changed', { status: 'interrupted', reason: 'process_restarted' }),
    )
    expect(summary.facts).toContainEqual({
      label: m.observation_event_reason(),
      value: m.observation_event_reason_process_restarted(),
    })
  })
})

describe('observation canonical item events', () => {
  test('an incomplete item keeps its title but warns and explains', () => {
    const summary = observationEventSummary(
      event(7, 'client_visible_content', { item: 'text:0', text: 'partial', complete: false }),
    )
    expect(summary.title).toBe(m.observation_event_client_visible_content())
    expect(summary.tone).toBe('warning')
    expect(summary.note).toBe(m.observation_event_content_incomplete())
    expect(summary.facts).toContainEqual({
      label: m.observation_event_result(),
      value: m.observation_event_incomplete(),
    })
  })

  test('a complete thinking item stays neutral for process grouping', () => {
    const summary = observationEventSummary(
      event(7, 'model_thinking', {
        model_turn_id: 'turn',
        attempt_id: 'attempt',
        item: 'thinking:0',
        text: 'done',
        complete: true,
      }),
    )
    expect(summary.title).toBe(m.observation_event_model_thinking())
    expect(summary.tone).toBe('neutral')
    expect(summary.note).toBeUndefined()
  })
})

describe('observation gap event summary', () => {
  const gap = (reason: string) => observationEventSummary(event(18, 'observation_gap', { reason }))

  test('explains a missing parent observation without claiming the request failed', () => {
    const summary = gap('generation_parent_observation_unavailable')
    expect(summary.tone).toBe('warning')
    expect(summary.note).toBe(m.observation_gap_generation_parent_unavailable())
    expect(summary.note).not.toContain(m.observation_status_failed())
    expect(summary.facts).toContainEqual({
      label: m.observation_event_reason(),
      value: 'generation_parent_observation_unavailable',
    })
  })

  test('explains unfinished observation activity without inventing an outcome', () => {
    const summary = gap('unfinished_observation_activity')
    expect(summary.tone).toBe('warning')
    expect(summary.note).toBe(m.observation_gap_unfinished_activity())
  })

  test('distinguishes delivered output whose history could not be saved from a request failure', () => {
    const reason = 'settlement_generation_commit:database busy'
    const summary = gap(reason)
    expect(summary.tone).toBe('warning')
    expect(summary.note).toBe(m.observation_gap_delivered_history_not_saved())
    expect(summary.note).not.toContain(m.observation_status_failed())
    expect(summary.facts).toContainEqual({ label: m.observation_event_reason(), value: reason })
  })

  test('keeps the generic incomplete-observation explanation for unknown reasons', () => {
    expect(gap('future_gap_reason').note).toBe(m.observation_event_gap_note())
  })
})

describe('observation request failed event summary', () => {
  const requestFailed = (error: Record<string, unknown>) =>
    observationEventSummary(event(20, 'request_failed', { error }))

  function factValue(facts: Array<{ label: string; value: string }>, label: string): string | undefined {
    return facts.find((fact) => fact.label === label)?.value
  }

  test('upstream terminal error renders as a regular error event with readable failure facts', () => {
    const summary = requestFailed({
      source: 'upstream',
      code: 'upstream_timeout',
      message: 'upstream connection reset while streaming',
      status_code: 504,
      upstream_code: null,
    })
    expect(summary.title).not.toBe(m.observation_event_unknown())
    expect(summary.title).toBeTruthy()
    expect(summary.note).toBeUndefined()
    expect(summary.tone).toBe('error')
    expect(factValue(summary.facts, m.failed_request_origin())).toBe(m.failed_request_upstream())
    expect(factValue(summary.facts, m.observation_http_status())).toBe('504')
    expect(factValue(summary.facts, m.observation_error_code())).toBe('upstream_timeout')
    expect(factValue(summary.facts, m.failed_request_error())).toBe('upstream connection reset while streaming')
  })

  test('platform terminal error renders without inventing an HTTP status', () => {
    const summary = requestFailed({
      source: 'platform',
      code: 'request_deadline_exceeded',
      message: 'request deadline exceeded',
    })
    expect(summary.title).not.toBe(m.observation_event_unknown())
    expect(summary.tone).toBe('error')
    expect(factValue(summary.facts, m.failed_request_origin())).toBe(m.failed_request_platform())
    expect(factValue(summary.facts, m.observation_error_code())).toBe('request_deadline_exceeded')
    expect(factValue(summary.facts, m.failed_request_error())).toBe('request deadline exceeded')
    expect(summary.facts.some((fact) => fact.label === m.observation_http_status())).toBe(false)
  })

  test('missing error source keeps the error event without fabricating an origin', () => {
    const summary = requestFailed({ code: 'bad_gateway', message: 'gateway closed the connection' })
    expect(summary.title).not.toBe(m.observation_event_unknown())
    expect(summary.tone).toBe('error')
    expect(summary.facts.some((fact) => fact.label === m.failed_request_origin())).toBe(false)
    expect(factValue(summary.facts, m.observation_error_code())).toBe('bad_gateway')
    expect(factValue(summary.facts, m.failed_request_error())).toBe('gateway closed the connection')
  })

  test('zh-CN renders a localized title and origin while failure facts stay readable', () => {
    const payload = {
      source: 'upstream',
      code: 'upstream_timeout',
      message: 'upstream connection reset while streaming',
      status_code: 504,
      upstream_code: null,
    }
    const english = requestFailed(payload)
    const original = getLocale
    overwriteGetLocale(() => 'zh-CN')
    try {
      const summary = requestFailed(payload)
      expect(summary.title).not.toBe(m.observation_event_unknown())
      expect(summary.title).not.toBe(english.title)
      expect(summary.tone).toBe('error')
      expect(factValue(summary.facts, m.failed_request_origin())).not.toBe(
        m.failed_request_upstream({}, { locale: 'en-US' }),
      )
      expect(factValue(summary.facts, m.observation_http_status())).toBe('504')
      expect(factValue(summary.facts, m.observation_error_code())).toBe('upstream_timeout')
      expect(factValue(summary.facts, m.failed_request_error())).toBe('upstream connection reset while streaming')
    } finally {
      overwriteGetLocale(original)
    }
  })
})
