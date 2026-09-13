import { describe, expect, test } from 'bun:test'
import { observationConversationActivities, type ToolActivity } from '../src/lib/observation-activities'
import { observationConversationMessages } from '../src/lib/observation-conversation'
import type { InteractionDetail, RunDetail } from '../src/lib/types/observation'

const usage = {
  input_tokens: null,
  output_tokens: null,
  cache_read_tokens: null,
  cache_write_tokens: null,
  reasoning_tokens: null,
}
function run(id: string, parent: string | null = null): RunDetail {
  return {
    id,
    parent_run_id: parent,
    generation_node_id: null,
    generation_parent_id: null,
    route_id: 'model',
    model_display_name: null,
    ingress_protocol: 'openai',
    status: 'running',
    terminal_reason: null,
    user_interrupted: false,
    debug_enabled: true,
    client_output_committed: true,
    started_at: 0,
    finished_at: null,
    usage,
    events: [],
    trace: null,
  }
}
function detail(runs: RunDetail[]): InteractionDetail {
  const interaction = {
    id: 'interaction',
    root_id: 'root',
    parent_interaction_id: null,
    generation_root_id: null,
    first_route_id: 'model',
    first_model_display_name: null,
    status: 'running',
    started_at: 0,
    last_active_at: 10,
    input_preview: 'Question',
    visible_tail: '',
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
function event(run: RunDetail, sequence: number, kind: string, payload: unknown) {
  run.events.push({
    run_id: run.id,
    interaction_id: 'interaction',
    rejection_id: null,
    sequence,
    occurred_at: sequence,
    kind,
    payload,
  })
}
function tools(value: InteractionDetail, id: string): ToolActivity[] {
  return observationConversationActivities(value)
    .get(`assistant:${id}`)!
    .filter((activity) => activity.kind === 'tool')
}

describe('observation activities', () => {
  test('accumulates ordinary thinking without Debug and stops failed attempts and runs before summary refresh', () => {
    const root = run('root')
    root.debug_enabled = false
    const scope = { model_turn_id: 'turn', attempt_id: 'attempt' }
    event(root, 1, 'model_thinking_delta', { ...scope, text: 'Read' })
    const value = detail([root])
    const first = observationConversationActivities(value)
      .get('assistant:root')!
      .find((activity) => activity.kind === 'thinking')!
    event(root, 2, 'model_thinking_delta', { ...scope, text: ' carefully', signature: 'opaque' })
    root.events.push(root.events[1])
    expect(observationConversationActivities(value).get('assistant:root')).toEqual([
      { ...first, text: 'Read carefully', live: true },
    ])
    event(root, 3, 'target_attempt_finished', { ...scope, status: 'failed' })
    event(root, 4, 'model_thinking_delta', { ...scope, attempt_id: 'retry', text: 'Retry' })
    expect(
      observationConversationActivities(value)
        .get('assistant:root')
        ?.map((activity) => activity.live),
    ).toEqual([false, true])
    event(root, 5, 'run_finished', { status: 'failed' })
    expect(
      observationConversationActivities(value)
        .get('assistant:root')
        ?.map((activity) => activity.live),
    ).toEqual([false, false])
    expect(observationConversationMessages(value).find((message) => message.id === 'assistant:root')?.text).toBe('')
  })

  test('resumes the same thinking marker without ending another active attempt', () => {
    const root = run('root')
    root.debug_enabled = false
    const scope = { model_turn_id: 'turn', attempt_id: 'attempt' }
    event(root, 1, 'model_thinking_delta', { ...scope, text: 'First.' })
    event(root, 2, 'model_thinking_finished', scope)
    const value = detail([root])
    const first = observationConversationActivities(value).get('assistant:root')![0]
    expect(first.live).toBe(false)
    event(root, 3, 'model_thinking_delta', { ...scope, text: ' Again.' })
    event(root, 4, 'model_thinking_delta', { ...scope, attempt_id: 'parallel', text: 'Independent.' })
    expect(observationConversationActivities(value).get('assistant:root')).toEqual([
      { ...first, text: 'First. Again.', live: true },
      expect.objectContaining({ text: 'Independent.', live: true }),
    ])
    event(root, 5, 'model_thinking_finished', scope)
    expect(
      observationConversationActivities(value)
        .get('assistant:root')
        ?.map((activity) => activity.live),
    ).toEqual([false, true])
  })

  test('preserves explicit null tool input and results without inventing missing content', () => {
    const root = run('root')
    event(root, 1, 'platform_tool_started', { model_turn_id: 'turn', tool_id: 'call', name: 'Bash', input: null })
    event(root, 4, 'platform_tool_finished', {
      model_turn_id: 'turn',
      tool_id: 'call',
      status: 'failed',
      content: null,
    })
    const value = detail([root])
    expect(tools(value, 'root')[0]).toMatchObject({
      input: null,
      live: false,
      results: [{ content: null, isError: true }],
    })
    root.debug_enabled = false
    expect(tools(value, 'root')[0]).toMatchObject({
      input: null,
      live: false,
      results: [{ content: null, isError: true }],
    })
    root.debug_enabled = true
    root.events[0].payload = { model_turn_id: 'turn', tool_id: 'call', name: 'Bash' }
    root.events[1].payload = { model_turn_id: 'turn', tool_id: 'call', status: 'completed' }
    expect(tools(value, 'root')[0]).not.toHaveProperty('input')
    expect(tools(value, 'root')[0]).toMatchObject({
      live: false,
      results: [],
    })
  })

  test('deduplicates ordinary client returns only within explicit ancestor branches', () => {
    const root = run('root')
    root.debug_enabled = false
    const left = run('left', 'root')
    const replay = run('replay', 'left')
    const right = run('right', 'root')
    const unrelated = run('unrelated')
    event(root, 1, 'client_tool_handoff', { tool_id: 'call', name: 'Bash', input: null })
    for (const child of [left, replay, right, unrelated]) {
      event(child, 2, 'client_tool_result', { tool_id: 'call', content: null, is_error: true })
    }
    event(left, 3, 'client_tool_result', { tool_id: 'call', content: null, is_error: true })
    const value = detail([replay, unrelated, right, left, root])
    expect(tools(value, 'root')[0].results.map(({ content, isError }) => ({ content, isError }))).toEqual([
      { content: null, isError: true },
      { content: null, isError: true },
    ])
    event(left, 4, 'client_tool_handoff', { tool_id: 'call', name: 'Bash' })
    expect(tools(value, 'left')[0].results.map((entry) => entry.content)).toEqual([null])
    expect(tools(value, 'root')[0].results).toHaveLength(2)
  })
  test('associates client returns only to explicit nearest ancestors, deduplicating replays but preserving sibling returns', () => {
    const root = run('root')
    const left = run('left', 'root')
    const replay = run('replay', 'left')
    const right = run('right', 'root')
    const unrelated = run('unrelated')
    const reused = run('reused', 'left')
    const reusedChild = run('reused-child', 'reused')
    for (const value of [root, unrelated, reused]) {
      event(value, 1, 'client_tool_handoff', { tool_id: 'call', name: 'Bash', input: { command: 'pwd' } })
    }
    for (const value of [left, replay, right]) {
      event(value, 2, 'client_tool_result', { tool_id: 'call', content: null, is_error: true })
    }
    event(left, 3, 'client_tool_result', { tool_id: 'call', content: null, is_error: true })
    event(unrelated, 2, 'client_tool_result', { tool_id: 'call', content: 'must not attach to self or sibling' })
    event(reusedChild, 2, 'client_tool_result', { tool_id: 'call', content: 'nearest call' })
    const value = detail([replay, reusedChild, unrelated, right, reused, left, root])
    const rootTool = tools(value, 'root')[0]
    expect(rootTool.results.map(({ content, isError }) => ({ content, isError }))).toEqual([
      { content: null, isError: true },
      { content: null, isError: true },
    ])
    expect(new Set(rootTool.results.map((entry) => entry.id)).size).toBe(2)
    expect(tools(value, 'unrelated')[0].results).toEqual([])
    expect(tools(value, 'reused')[0].results.map((entry) => entry.content)).toEqual(['nearest call'])
    expect(tools(value, 'reused')[0].id).not.toBe(rootTool.id)
  })
})
