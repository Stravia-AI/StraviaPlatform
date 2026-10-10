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
    delivery_completed_at: null,
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
    failed_request: false,
    client_output_delivered: false,
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
  test('keeps handed-off tools waiting after HTTP delivery and closes only returned calls or authoritative waiting', () => {
    const root = run('root')
    root.status = 'waiting_client'
    root.delivery_completed_at = 3
    event(root, 1, 'client_tool_handoff', { tool_id: 'a', name: 'Read' })
    event(root, 2, 'client_tool_handoff', { tool_id: 'b', name: 'Read' })
    event(root, 3, 'run_finished', { status: 'waiting_client' })
    const value = detail([root])
    value.interaction.status = 'completed'
    expect(tools(value, 'root')).toMatchObject([
      { live: true, status: 'waiting' },
      { live: true, status: 'waiting' },
    ])
    const child = run('child', 'root')
    event(child, 4, 'client_tool_result', { tool_id: 'a', content: 'failure', is_error: true })
    value.runs.push(child)
    expect(tools(value, 'root')).toMatchObject([
      { live: false, status: 'error', results: [{ content: 'failure', isError: true }] },
      { live: true, status: 'waiting' },
    ])
    root.status = 'superseded'
    root.terminal_reason = 'superseded'
    expect(tools(value, 'root')[1]).toMatchObject({ live: false, status: 'missing-result', reason: 'superseded' })
    event(child, 5, 'client_tool_result', { tool_id: 'b', content: 'late return' })
    expect(tools(value, 'root')[1]).toMatchObject({
      live: false,
      status: 'returned',
      results: [{ content: 'late return' }],
    })
  })

  test('shows authoritative waiting closure reasons without claiming a client execution failure', () => {
    for (const reason of ['client_disconnected', 'client_wait_expired', 'process_restarted', 'user_interrupted']) {
      const root = run('root')
      root.status = 'interrupted'
      root.terminal_reason = reason
      event(root, 1, 'client_tool_handoff', { tool_id: 'call', name: 'Bash' })
      expect(tools(detail([root]), 'root')[0]).toMatchObject({
        live: false,
        status: 'missing-result',
        reason,
        results: [],
      })
      const child = run('late', 'root')
      event(child, 2, 'client_tool_result', { tool_id: 'call', content: 'late result', is_error: true })
      expect(tools(detail([root, child]), 'root')[0]).toMatchObject({
        live: false,
        status: 'error',
        reason,
        results: [{ content: 'late result', isError: true }],
      })
    }
  })

  test('does not guess which same-ID call owns a result when one source Run contains ambiguous calls', () => {
    const root = run('root')
    root.status = 'waiting_client'
    event(root, 1, 'client_tool_handoff', { model_turn_id: 'first', tool_id: 'same', name: 'Read' })
    event(root, 2, 'client_tool_handoff', { model_turn_id: 'second', tool_id: 'same', name: 'Read' })
    const child = run('child', 'root')
    event(child, 3, 'client_tool_result', { tool_id: 'same', content: 'cannot prove owner' })
    const activities = tools(detail([root, child]), 'root')
    expect(activities[0].id).not.toBe(activities[1].id)
    expect(activities).toMatchObject([
      { live: true, results: [] },
      { live: true, results: [] },
    ])
  })

  test('platform execution outlives response completion and stops on its own finish or missing-observation evidence', () => {
    const root = run('root')
    root.status = 'completed'
    event(root, 1, 'platform_tool_started', { model_turn_id: 'turn', tool_id: 'call', name: 'Search' })
    event(root, 2, 'run_finished', { status: 'completed' })
    const value = detail([root])
    expect(tools(value, 'root')[0]).toMatchObject({ live: true, status: 'running' })
    event(root, 3, 'platform_tool_finished', { model_turn_id: 'turn', tool_id: 'call', status: 'failed' })
    expect(tools(value, 'root')[0]).toMatchObject({ live: false, status: 'error', results: [] })
    root.events.pop()
    event(root, 3, 'observation_gap', { reason: 'unfinished_observation_activity' })
    expect(tools(value, 'root')[0]).toMatchObject({ live: false, status: 'missing-result', results: [] })
    root.events.pop()
    event(root, 3, 'run_state_changed', { status: 'completed', reason: 'process_restarted' })
    expect(tools(value, 'root')[0]).toMatchObject({
      live: false,
      status: 'missing-result',
      reason: 'process_restarted',
      results: [],
    })
  })

  test('reconciles a durable thinking item with its live block without losing paragraphs', () => {
    const root = run('root')
    root.debug_enabled = false
    const value = detail([root])
    const text = '**Selecting top five candidate features**\n\n**Implementing temp path and timestamp retrieval**'
    const block = {
      block_id: 'thinking',
      interaction_id: 'interaction',
      run_id: root.id,
      kind: 'model_thinking_delta' as const,
      model_turn_id: 'turn',
      attempt_id: 'attempt',
      occurred_at: 1,
      revision: 1,
      text,
    }
    const live = observationConversationActivities(value, [block]).get('assistant:root')![0]
    expect(live).toMatchObject({ text, live: true })
    event(root, 1, 'model_thinking', {
      model_turn_id: 'turn',
      attempt_id: 'attempt',
      block_id: block.block_id,
      item: 'thinking:0',
      text,
      parts: [{ type: 'text', text }],
      complete: true,
    })
    expect(observationConversationActivities(value, [block]).get('assistant:root')).toEqual([{ ...live, live: false }])
    expect(observationConversationActivities(value).get('assistant:root')).toEqual([{ ...live, live: false }])
    expect(observationConversationMessages(value).find((message) => message.id === 'assistant:root')?.text).toBe('')
  })

  test('keeps partial failed thinking durable and reconciles retries independently', () => {
    const root = run('root')
    root.debug_enabled = false
    const value = detail([root])
    event(root, 1, 'model_thinking', {
      model_turn_id: 'turn',
      attempt_id: 'attempt',
      block_id: 'first',
      item: 'thinking:0',
      text: 'Read carefully',
      parts: [{ type: 'text', text: 'Read carefully' }],
      complete: false,
    })
    root.events.push(root.events[0])
    event(root, 2, 'target_attempt_finished', {
      model_turn_id: 'turn',
      attempt_id: 'attempt',
      status: 'failed',
      usage: null,
    })
    const retry = {
      block_id: 'retry',
      interaction_id: 'interaction',
      run_id: root.id,
      kind: 'model_thinking_delta' as const,
      model_turn_id: 'turn',
      attempt_id: 'retry',
      occurred_at: 3,
      revision: 1,
      text: 'Retry',
    }
    expect(observationConversationActivities(value, [retry]).get('assistant:root')).toMatchObject([
      { text: 'Read carefully', live: false },
      { text: 'Retry', live: true },
    ])
    event(root, 4, 'model_thinking', {
      model_turn_id: 'turn',
      attempt_id: 'retry',
      block_id: 'retry',
      item: 'thinking:1',
      text: 'Retry',
      parts: [{ type: 'text', text: 'Retry' }],
      complete: false,
    })
    event(root, 5, 'run_finished', { status: 'failed', delivery: null })
    expect(observationConversationActivities(value, [retry]).get('assistant:root')).toMatchObject([
      { text: 'Read carefully', live: false },
      { text: 'Retry', live: false },
    ])
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
    expect(tools(value, 'root')[0]).toMatchObject({ live: false, results: [] })
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
    event(replay, 5, 'client_tool_result', { tool_id: 'call', content: null, is_error: true })
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
