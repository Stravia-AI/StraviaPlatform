import { describe, expect, test } from 'bun:test'

import { buildLatencyChart } from '../src/lib/stats-chart'

describe('latency chart', () => {
  test('breaks both series across hours without requests', () => {
    const points = buildLatencyChart(
      [
        { hour: '2026-09-02 18:00:00', avg_first_token_ms: 1_000, avg_duration_ms: 5_000 },
        { hour: '2026-09-02 20:00:00', avg_first_token_ms: 2_000, avg_duration_ms: 6_000 },
        { hour: '2026-09-02 21:00:00', avg_first_token_ms: null, avg_duration_ms: 7_000 },
      ],
      (hour) => hour,
    )

    expect(points).toEqual([
      { bucket: '2026-09-02 18:00:00', firstToken: 1, duration: 5 },
      { bucket: '2026-09-02 19:00:00', firstToken: null, duration: null },
      { bucket: '2026-09-02 20:00:00', firstToken: 2, duration: 6 },
      { bucket: '2026-09-02 21:00:00', firstToken: null, duration: 7 },
    ])
  })
})
