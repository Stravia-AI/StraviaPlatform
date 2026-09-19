import { describe, expect, test } from 'bun:test'
import { observationAttemptOutputTokens, observationEventSummary } from '../src/lib/observation-event-summary'
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

const finished = event(10, 'target_attempt_finished', {
  attempt_id: 'current',
  status: 'completed',
  duration_ms: 18_750,
  first_token_ms: 7_650,
})

function speed(events: ObservationEvent[], completion = finished): string | undefined {
  return observationEventSummary(completion, observationAttemptOutputTokens(events)).facts.find(
    (fact) => fact.label === m.logs_token_speed(),
  )?.value
}

describe('observation attempt output speed', () => {
  test('uses the latest cumulative usage for this attempt rather than another attempt or the sum', () => {
    expect(
      speed([
        event(3, 'usage_confirmed', { attempt_id: 'current', usage: { output_tokens: 1110 } }),
        event(1, 'usage_confirmed', { attempt_id: 'current', usage: { output_tokens: 100 } }),
        event(4, 'usage_confirmed', { attempt_id: 'other', usage: { output_tokens: 9999 } }),
      ]),
    ).toBe('100 tok/s')
  })

  test('does not invent throughput when matching usage is missing or its latest value is unknown', () => {
    const foreign = event(1, 'usage_confirmed', { attempt_id: 'other', usage: { output_tokens: 1110 } })
    expect(speed([foreign])).toBe('–')
    expect(
      speed([
        foreign,
        event(2, 'usage_confirmed', { attempt_id: 'current', usage: { output_tokens: 1110 } }),
        event(3, 'usage_confirmed', { attempt_id: 'current', usage: { output_tokens: null } }),
      ]),
    ).toBe('–')
  })
})

describe('observation usage event summary', () => {
  test('keeps cache counters separate without exposing reasoning as another output counter', () => {
    const summary = observationEventSummary(
      event(5, 'usage_confirmed', {
        usage: {
          input_tokens: 920,
          output_tokens: 86,
          cache_read_tokens: 320,
          cache_write_tokens: 12,
          reasoning_tokens: 4_444,
        },
      }),
    )

    expect(summary.facts).toEqual([
      { label: m.observation_event_tokens_input(), value: '920' },
      { label: m.observation_event_tokens_output(), value: '86' },
      { label: m.observation_event_tokens_cache_read(), value: '320' },
      { label: m.observation_event_tokens_cache_write(), value: '12' },
    ])
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
      upstream_code: null,    })
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
      upstream_code: null,    }
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
