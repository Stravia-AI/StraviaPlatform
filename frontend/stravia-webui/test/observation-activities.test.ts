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
    debug_events: [],
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
  }
}
function trace(run: RunDetail, sequence: number, stage: string, payload: unknown, turn: string | null = 'turn') {
  const record = {
    run_id: run.id,
    interaction_id: null,
    sequence,
    recorded_at: sequence,
    layer: 'canonical',
    payload_encoding: 'json',
    stage,
    payload,
    model_turn_id: turn,
    attempt_id: stage === 'canonical_delta' || stage === 'canonical_terminal_response' ? 'attempt' : null,
  }
  run.debug_events.push(record)
  return record
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
function response(content: unknown[]) {
  return { items: [{ role: 'assistant', content }] }
}
function tool(run: RunDetail, id = 'call', content: unknown = { command: 'pwd' }) {
  trace(run, 1, 'response_after_hook', response([{ type: 'tool_use', id, name: 'Bash', input: content }]))
}
function result(run: RunDetail, content: unknown, id = 'call', sequence = 2) {
  trace(
    run,
    sequence,
    'decoded_request',
    { items: [{ role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content, is_error: true }] }] },
    null,
  )
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

  test('ordinary thinking supersedes Debug deltas and snapshots in only the same attempt', () => {
    const root = run('root')
    const scope = { model_turn_id: 'turn', attempt_id: 'attempt' }
    trace(root, 1, 'canonical_delta', { kind: 'thinking_delta', data: 'Debug' })
    const value = detail([root])
    const first = observationConversationActivities(value)
      .get('assistant:root')!
      .find((activity) => activity.kind === 'thinking')!
    event(root, 2, 'model_thinking_delta', { ...scope, text: 'Ordinary' })
    event(root, 3, 'model_thinking_finished', scope)
    trace(
      root,
      4,
      'canonical_terminal_response',
      response([{ type: 'thinking', thinking: 'Snapshot', signature: 'opaque' }]),
    )
    trace(
      root,
      5,
      'response_after_hook',
      response([{ type: 'reasoning', summary: ['Snapshot'], encrypted_content: 'opaque' }]),
    )
    const retry = trace(root, 6, 'canonical_delta', { kind: 'thinking_delta', data: 'Legacy retry' })
    retry.attempt_id = 'retry'
    expect(observationConversationActivities(value).get('assistant:root')).toEqual([
      { ...first, at: 2, text: 'Ordinary', live: false },
      {
        kind: 'thinking',
        id: JSON.stringify(['interaction', 'root', 'thinking', JSON.stringify(['turn', 'retry'])]),
        at: 6,
        text: 'Legacy retry',
        live: true,
      },
    ])
  })

  test('ordinary platform input and result null override Debug while missing fields retain legacy content', () => {
    const root = run('root')
    event(root, 1, 'platform_tool_started', { model_turn_id: 'turn', tool_id: 'call', name: 'Bash', input: null })
    trace(root, 2, 'platform_tool_call', { id: 'call', name: 'Bash', arguments: '{"debug":true}' })
    trace(root, 3, 'platform_tool_result', { call_id: 'call', content: 'Debug result', is_error: false })
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
    expect(tools(value, 'root')[0]).toMatchObject({
      input: { debug: true },
      results: [{ content: 'Debug result', isError: false }],
    })
  })

  test('reads explicit legacy role tool text without interpreting business JSON or duplicating ordinary returns', () => {
    const root = run('root')
    const child = run('child', 'root')
    tool(root)
    const content = '{"type":"tool_result","tool_use_id":"nested","content":"business data"}'
    trace(
      child,
      2,
      'decoded_request',
      {
        items: [
          { role: 'tool', tool_call_id: 'call', content },
          { role: 'user', tool_call_id: 'call', content: 'not a tool return' },
          { role: 'tool', content: 'no explicit call' },
        ],
      },
      null,
    )
    const value = detail([root, child])
    expect(tools(value, 'root')[0].results.map((entry) => entry.content)).toEqual([content])
    event(child, 3, 'client_tool_result', { tool_id: 'call', content: null, is_error: false })
    expect(tools(value, 'root')[0].results.map((entry) => entry.content)).toEqual([null])
  })

  test('ordinary client returns override Debug and deduplicate only within explicit ancestor branches', () => {
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
    result(left, 'Debug must not duplicate or override ordinary null')
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
  test('replaces streamed thinking and arguments with terminal snapshots without changing identities or visible text', () => {
    const root = run('root')
    trace(root, 1, 'canonical_delta', { kind: 'thinking_delta', data: 'Look' })
    trace(root, 2, 'canonical_delta', { kind: 'thinking_delta', data: ' ' })
    trace(root, 3, 'canonical_delta', {
      kind: 'thinking_delta_with_metadata',
      data: { text: 'carefully', output_index: 0 },
    })
    const value = detail([root])
    const first = observationConversationActivities(value).get('assistant:root')!
    expect(first[0]).toMatchObject({ text: 'Look carefully', live: true })
    trace(root, 4, 'canonical_delta', { kind: 'tool_call_start', data: { index: 0, id: 'call', name: 'Bash' } })
    trace(root, 5, 'canonical_delta', { kind: 'tool_call_delta', data: { index: 0, arguments: '{"command":' } })
    trace(root, 6, 'canonical_delta', { kind: 'tool_call_delta', data: { index: 0, arguments: '"pwd"}' } })
    const streamedTool = tools(value, 'root')[0]
    expect(streamedTool.input).toEqual({ command: 'pwd' })
    trace(root, 7, 'canonical_delta', {
      kind: 'tool_call_complete',
      data: { index: 0, tool_call: { id: 'call', name: 'Bash', arguments: '{"command":"pwd"}' } },
    })
    const terminal = response([
      { type: 'thinking', thinking: 'Look carefully', signature: 'secret-signature' },
      { type: 'tool_use', id: 'call', name: 'Bash', input: { command: 'pwd' } },
    ])
    trace(root, 8, 'canonical_terminal_response', terminal)
    trace(root, 9, 'response_after_hook', terminal)
    root.status = 'completed'
    const final = observationConversationActivities(value).get('assistant:root')!
    expect(final).toEqual([
      { ...first[0], live: false },
      { ...streamedTool, live: false },
    ])
    expect(observationConversationMessages(value).find((message) => message.id === 'assistant:root')?.text).toBe('')
  })

  test('reads only explicit public canonical blocks, with debug/run gates and no opaque or wire interpretation', () => {
    const root = run('root')
    trace(
      root,
      1,
      'canonical_terminal_response',
      response([
        { type: 'thinking', thinking: 'Readable', signature: 'opaque' },
        { type: 'reasoning', content: ['Detail'], summary: ['Summary'], encrypted_content: 'opaque' },
        { type: 'redacted_thinking', data: 'opaque' },
        { type: 'tool_use', id: 'call', name: 'Bash', input: { type: 'thinking', thinking: 'business data' } },
      ]),
    )
    const wrongRun = trace(root, 2, 'canonical_delta', { kind: 'thinking_delta', data: 'wrong run' })
    wrongRun.run_id = 'other'
    const wire = trace(root, 3, 'canonical_delta', { kind: 'thinking_delta', data: 'wire' })
    wire.layer = 'wire'
    trace(root, 4, 'canonical_delta', null)
    trace(root, 5, 'canonical_delta', { kind: 'thinking_signature', data: 'signature' })
    const value = detail([root])
    expect(observationConversationActivities(value).get('assistant:root')?.[0]).toMatchObject({
      text: 'Readable\n\nDetail\n\nSummary',
      live: false,
    })
    expect(tools(value, 'root')[0].input).toEqual({ type: 'thinking', thinking: 'business data' })
    root.debug_enabled = false
    root.events.push({
      run_id: root.id,
      interaction_id: 'interaction',
      rejection_id: null,
      sequence: 1,
      occurred_at: 1,
      kind: 'client_tool_handoff',
      payload: { tool_id: 'call', name: 'Bash', input: null },
    })
    expect(observationConversationActivities(value).get('assistant:root')).toEqual([
      { kind: 'tool', id: tools(value, 'root')[0].id, at: 1, name: 'Bash', live: false, input: null, results: [] },
    ])
  })

  test('uses call_id for platform results, preserves null, and retains marker identity when debug arrives', () => {
    const root = run('root')
    root.events.push({
      run_id: root.id,
      interaction_id: 'interaction',
      rejection_id: null,
      sequence: 1,
      occurred_at: 1,
      kind: 'platform_tool_started',
      payload: { model_turn_id: 'turn', tool_id: 'call', name: 'Bash' },
    })
    const value = detail([root])
    const initial = tools(value, 'root')[0]
    trace(root, 2, 'canonical_delta', { kind: 'tool_call_start', data: { index: 0, id: 'call', name: 'Bash' } })
    trace(root, 3, 'platform_tool_call', { id: 'call', name: 'Bash', arguments: 'null' })
    trace(root, 4, 'platform_tool_result', { tool_id: 'call', call_id: 'unrelated', content: 'wrong' })
    const complete = trace(root, 5, 'platform_tool_result', {
      tool_id: 'bash',
      call_id: 'call',
      content: null,
      is_error: true,
    })
    root.debug_events.push(complete)
    const activities = tools(value, 'root')
    expect(activities).toHaveLength(1)
    expect(activities[0]).toMatchObject({
      id: initial.id,
      input: null,
      live: false,
      results: [{ source: 'platform', content: null, isError: true }],
    })
  })

  test('associates client returns only to explicit nearest ancestors, deduplicating replays but preserving sibling returns', () => {
    const root = run('root')
    const left = run('left', 'root')
    const replay = run('replay', 'left')
    const right = run('right', 'root')
    const unrelated = run('unrelated')
    const reused = run('reused', 'left')
    const reusedChild = run('reused-child', 'reused')
    tool(root)
    tool(unrelated)
    tool(reused)
    result(left, null)
    result(left, null, 'call', 3)
    result(replay, null)
    result(right, null)
    result(unrelated, 'must not attach to self or sibling')
    result(reusedChild, 'nearest call')
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
