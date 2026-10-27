import { describe, expect, test } from 'bun:test'
import { resolveCredentialHits } from '../src/lib/credential-hit-highlights'
import type { ToolActivity } from '../src/lib/observation-activities'
import type { InteractionDetail, ObservationEvent, RunDetail } from '../src/lib/types/observation'

const usage = {
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_write_tokens: null,
  reasoning_tokens: null,
}

function hitEvent(runId: string, sequence: number, kind: string, discoveries: unknown[]): ObservationEvent {
  return {
    sequence,
    occurred_at: sequence,
    interaction_id: 'interaction',
    run_id: runId,
    rejection_id: null,
    kind,
    payload: { discoveries },
  }
}

function run(id: string, startedAt: number, events: ObservationEvent[]): RunDetail {
  return {
    id,
    parent_run_id: null,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: 'model',
    model_display_name: null,
    ingress_protocol: 'openai',
    status: 'completed',
    terminal_reason: null,
    user_interrupted: false,
    debug_enabled: false,
    client_output_committed: true,
    started_at: startedAt,
    finished_at: null,
    delivery_completed_at: null,
    usage,
    events,
    trace: null,
  }
}

function detail(runs: RunDetail[]): InteractionDetail {
  return { runs } as InteractionDetail
}

function tool(callId: string, withResult: boolean): ToolActivity {
  return {
    kind: 'tool',
    id: `tool:${callId}`,
    callId,
    at: 0,
    name: 'shell',
    live: false,
    status: 'returned',
    results: withResult ? [{ id: 'result', at: 0, source: 'client', content: 'ok', isError: false }] : [],
  }
}

const discovery = (locators: unknown[]) => ({ rule_ids: [], source_types: [], locators })
const locator = (rule: string, source: string, extra: Record<string, unknown> = {}) => ({
  rule_id: rule,
  source_type: source,
  ...extra,
})

describe('resolveCredentialHits', () => {
  test('locates the latest input, tool calls and tool results, preferring new over reused', () => {
    const runs = [
      run('run-1', 1, [
        hitEvent('run-1', 1, 'credential_mappings_created', [
          discovery([locator('github-pat', 'user_message', { latest_input: true })]),
        ]),
      ]),
      run('run-2', 2, [
        hitEvent('run-2', 2, 'credential_mappings_created', [
          discovery([locator('aws-key', 'tool_arguments', { tool_id: 'call-1' })]),
        ]),
        hitEvent('run-2', 3, 'credential_mappings_reused', [
          discovery([
            locator('aws-key', 'tool_arguments', { tool_id: 'call-1' }),
            locator('slack-token', 'tool_result', { tool_id: 'call-1' }),
          ]),
        ]),
      ]),
    ]
    const hits = resolveCredentialHits(detail(runs), {
      userMessageIdByRun: new Map([['run-1', 'user:interaction']]),
      activities: [tool('call-1', true)],
    })

    expect(hits.userMessages.get('user:interaction')).toEqual({ kind: 'created', ruleIds: ['github-pat'] })
    expect(hits.toolCalls.get('call-1')).toEqual({ kind: 'created', ruleIds: ['aws-key'] })
    expect(hits.toolResults.get('call-1')).toEqual({ kind: 'reused', ruleIds: ['slack-token'] })
    expect(hits.unresolved).toEqual([])
    expect([hits.created, hits.reused]).toEqual([2, 1])
  })

  test('resolves the replayed latest input of a tool continuation to the message that run inherited', () => {
    const replayed = (runId: string, sequence: number) =>
      hitEvent(runId, sequence, 'credential_mappings_reused', [
        discovery([locator('github-pat', 'user_message', { latest_input: true })]),
      ])
    const hits = resolveCredentialHits(
      detail([
        run('run-1', 1, [replayed('run-1', 1)]),
        // 工具续跑：没有新的用户输入，也就没有自己的消息。
        run('run-2', 2, [replayed('run-2', 2)]),
        run('run-3', 3, [replayed('run-3', 3)]),
      ]),
      {
        userMessageIdByRun: new Map([
          ['run-1', 'user:first'],
          ['run-3', 'user:second'],
        ]),
        activities: [],
      },
    )

    expect([...hits.userMessages]).toEqual([
      ['user:first', { kind: 'reused', ruleIds: ['github-pat'] }],
      ['user:second', { kind: 'reused', ruleIds: ['github-pat'] }],
    ])
    expect(hits.unresolved).toEqual([])
    expect([hits.created, hits.reused]).toEqual([0, 2])
  })

  test('counts replayed history once and points at the first run that reported it', () => {
    const replay = (runId: string, sequence: number) =>
      hitEvent(runId, sequence, 'credential_mappings_reused', [
        discovery([locator('github-pat', 'user_message', { latest_input: false })]),
      ])
    const hits = resolveCredentialHits(
      detail([run('run-2', 2, [replay('run-2', 20)]), run('run-1', 1, [replay('run-1', 10)])]),
      { userMessageIdByRun: new Map(), activities: [] },
    )

    expect(hits.unresolved).toEqual([{ runId: 'run-1', mark: { kind: 'reused', ruleIds: ['github-pat'] } }])
    expect([hits.created, hits.reused]).toEqual([0, 1])
  })

  test('sends hits whose block cannot be found to the run instead of guessing', () => {
    const hits = resolveCredentialHits(
      detail([
        run('run-1', 1, [
          hitEvent('run-1', 1, 'credential_mappings_created', [
            discovery([
              locator('aws-key', 'tool_arguments', { tool_id: 'earlier-interaction' }),
              locator('slack-token', 'tool_result', { tool_id: 'call-1' }),
              locator('github-pat', 'user_message', { latest_input: true }),
              locator('jwt', 'system_or_history'),
            ]),
          ]),
        ]),
      ]),
      // call-1 has no result block; the run has no visible user message.
      { userMessageIdByRun: new Map(), activities: [tool('call-1', false)] },
    )

    expect(hits.userMessages.size + hits.toolCalls.size + hits.toolResults.size).toBe(0)
    expect(hits.unresolved.map((hit) => hit.mark.ruleIds[0]).sort()).toEqual([
      'aws-key',
      'github-pat',
      'jwt',
      'slack-token',
    ])
    expect([hits.created, hits.reused]).toEqual([4, 0])
  })

  test('ignores events recorded before locators existed', () => {
    const legacy = { rule_ids: ['github-pat'], source_types: ['user_message', 'tool_result'] }
    const hits = resolveCredentialHits(
      detail([run('run-1', 1, [hitEvent('run-1', 1, 'credential_mappings_created', [legacy])])]),
      { userMessageIdByRun: new Map([['run-1', 'user:interaction']]), activities: [] },
    )

    expect([hits.created, hits.reused, hits.unresolved.length]).toEqual([0, 0, 0])
    expect(hits.userMessages.size).toBe(0)
  })

  test('only reads a run’s own hit events', () => {
    const foreign = hitEvent('run-2', 1, 'credential_mappings_created', [
      discovery([locator('github-pat', 'user_message', { latest_input: true })]),
    ])
    const hits = resolveCredentialHits(detail([run('run-1', 1, [foreign])]), {
      userMessageIdByRun: new Map([['run-1', 'user:interaction']]),
      activities: [],
    })

    expect([hits.created, hits.reused]).toEqual([0, 0])
  })
})
