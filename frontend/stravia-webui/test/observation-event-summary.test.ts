import { describe, expect, test } from 'bun:test'
import { observationAttemptOutputTokens, observationEventSummary } from '../src/lib/observation-event-summary'
import * as m from '../src/lib/paraglide/messages.js'
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
