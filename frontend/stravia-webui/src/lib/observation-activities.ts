import type { InteractionDetail, RunDetail } from './types/observation'

export interface ThinkingActivity {
  kind: 'thinking'
  id: string
  at: number
  text: string
  live: boolean
}
export interface ActivityToolResult {
  id: string
  at: number
  source: 'client' | 'platform'
  content: unknown
  isError: boolean
}
export interface ToolActivity {
  kind: 'tool'
  id: string
  at: number
  name: string
  live: boolean
  input?: unknown
  results: ActivityToolResult[]
}
export type ObservationActivity = ThinkingActivity | ToolActivity

type ObjectValue = Record<string, unknown>
type Trace = ObjectValue & { sequence: number; recorded_at: number; stage: string }
interface Call {
  callId: string
  modelTurnId: string
  activity: ToolActivity
  arguments: string
}
interface ClientResult {
  run: RunDetail
  sequence: number
  at: number
  block: ObjectValue
  index: number
}

function object(value: unknown): ObjectValue | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as ObjectValue) : undefined
}
function string(value: unknown): string {
  return typeof value === 'string' ? value : ''
}
function input(value: string): unknown {
  try {
    return JSON.parse(value)
  } catch {
    return value
  }
}
function identity(...parts: (string | number)[]): string {
  return JSON.stringify(parts)
}
function blocks(payload: ObjectValue): ObjectValue[] {
  const result: ObjectValue[] = []
  if (!Array.isArray(payload.items)) return result
  for (const item of payload.items) {
    const content = object(item)?.content
    if (!Array.isArray(content)) continue
    for (const value of content) {
      const block = object(value)
      if (block) result.push(block)
    }
  }
  return result
}
function thinkingText(block: ObjectValue): string {
  if (block.type === 'thinking') return string(block.thinking)
  if (block.type !== 'reasoning') return ''
  return [block.content, block.summary]
    .flatMap((parts) => (Array.isArray(parts) ? parts.filter((part) => typeof part === 'string') : []))
    .join('\n\n')
}
function traces(run: RunDetail): Trace[] {
  if (!run.debug_enabled) return []
  return run.debug_events
    .flatMap((value) => {
      const record = object(value)
      if (
        !record ||
        record.run_id !== run.id ||
        record.layer !== 'canonical' ||
        record.payload_encoding !== 'json' ||
        typeof record.stage !== 'string' ||
        typeof record.sequence !== 'number' ||
        typeof record.recorded_at !== 'number'
      )
        return []
      return [record as Trace]
    })
    .sort((a, b) => a.sequence - b.sequence)
}

/** 普通事件优先，旧 canonical Debug 仅补缺失内容；不混入已交付的对话正文。 */
export function observationConversationActivities(detail: InteractionDetail): Map<string, ObservationActivity[]> {
  const output = new Map<string, ObservationActivity[]>()
  const callsByRun = new Map<string, Call[]>()
  const runsById = new Map(detail.runs.map((run) => [run.id, run]))
  const clientResults: ClientResult[] = []

  for (const run of detail.runs) {
    const activities: ObservationActivity[] = []
    const calls: Call[] = []
    const thoughts = new Map<string, ThinkingActivity>()
    const indexes = new Map<string, Call>()
    const seen = new Set<number>()
    const prefix = [detail.interaction.id, run.id]
    let currentScope = identity('', '')
    let currentTurn = ''
    let currentAttempt = ''
    const events = [
      ...new Map(
        run.events.filter((event) => event.run_id === run.id).map((event) => [event.sequence, event]),
      ).values(),
    ].sort((a, b) => a.sequence - b.sequence)
    const scopeOf = (payload: ObjectValue) => identity(string(payload.model_turn_id), string(payload.attempt_id))
    const ordinaryThoughts = new Set<string>()
    const ordinaryClientResults = new Set<string>()
    const finishedThoughts = new Set<string>()
    const finishedAttempts = new Set<string>()
    const finishedTurns = new Set<string>()
    for (const event of events) {
      const payload = object(event.payload)
      if (!payload) continue
      if (event.kind === 'model_thinking_delta' && string(payload.text)) {
        ordinaryThoughts.add(scopeOf(payload))
        finishedThoughts.delete(scopeOf(payload))
      }
      if (event.kind === 'model_thinking_finished') finishedThoughts.add(scopeOf(payload))
      if (event.kind === 'target_attempt_finished') finishedAttempts.add(scopeOf(payload))
      if (event.kind === 'model_turn_finished') finishedTurns.add(string(payload.model_turn_id))
      if (event.kind === 'client_tool_result' && Object.hasOwn(payload, 'content')) {
        ordinaryClientResults.add(string(payload.tool_id))
      }
    }
    const running =
      run.status === 'running' &&
      !events.some(
        (event) =>
          event.kind === 'run_finished' ||
          (event.kind === 'run_state_changed' && object(event.payload)?.status !== 'running'),
      )
    const stopThinking = () => {
      for (const thought of thoughts.values()) thought.live = false
    }
    const call = (id: string, name: string, modelTurnId: string, at: number): Call | undefined => {
      if (!id || !name) return undefined
      let existing = calls.find((entry) => entry.callId === id)
      if (!existing) {
        const activity: ToolActivity = {
          kind: 'tool',
          id: identity(...prefix, 'tool', id),
          at,
          name,
          live: running,
          results: [],
        }
        existing = { callId: id, modelTurnId, activity, arguments: '' }
        calls.push(existing)
        activities.push(activity)
      }
      existing.activity.name = name
      return existing
    }
    const thought = (scope: string, text: string, at: number, snapshot: boolean) => {
      if (!text) return
      let activity = thoughts.get(scope)
      if (!activity) {
        activity = { kind: 'thinking', id: identity(...prefix, 'thinking', scope), at, text: '', live: running }
        thoughts.set(scope, activity)
        activities.push(activity)
      }
      // 终态是完整快照，不是另一个增量；独立空白增量也属于原文。
      activity.text = snapshot ? text : activity.text + text
      activity.live =
        running &&
        !snapshot &&
        !finishedTurns.has(currentTurn) &&
        !finishedAttempts.has(scope) &&
        !finishedThoughts.has(scope)
    }

    for (const trace of traces(run)) {
      if (seen.has(trace.sequence)) continue
      seen.add(trace.sequence)
      const payload = object(trace.payload)
      if (!payload) continue
      const explicitScope = typeof trace.model_turn_id === 'string'
      const turn = explicitScope ? string(trace.model_turn_id) : currentTurn
      // response_after_hook 不带 attempt_id，仍属于该 Model Turn 最近的尝试。
      const ordinaryAttempt =
        typeof trace.attempt_id === 'string'
          ? undefined
          : events.findLast(
              (event) =>
                event.occurred_at <= trace.recorded_at &&
                (event.kind === 'target_attempt_started' ||
                  event.kind === 'model_thinking_delta' ||
                  event.kind === 'model_thinking_finished') &&
                object(event.payload)?.model_turn_id === turn,
            )
      const attempt =
        typeof trace.attempt_id === 'string'
          ? trace.attempt_id
          : string(object(ordinaryAttempt?.payload)?.attempt_id) || (turn === currentTurn ? currentAttempt : '')
      const scope = identity(turn, attempt)
      if (explicitScope && scope !== currentScope) stopThinking()
      if (explicitScope) {
        currentScope = scope
        currentTurn = turn
        currentAttempt = attempt
      }
      if (trace.stage === 'canonical_delta') {
        const data = object(payload.data)
        const kind = payload.kind
        if (kind === 'thinking_delta') {
          if (!ordinaryThoughts.has(scope)) thought(scope, string(payload.data), trace.recorded_at, false)
        } else if (kind === 'thinking_delta_with_metadata' || kind === 'reasoning_summary_delta') {
          if (!ordinaryThoughts.has(scope)) thought(scope, string(data?.text), trace.recorded_at, false)
        } else if (kind === 'tool_call_start' && data) {
          stopThinking()
          const entry = call(string(data.id), string(data.name), turn, trace.recorded_at)
          if (entry && typeof data.index === 'number') indexes.set(identity(scope, data.index), entry)
        } else if (kind === 'tool_call_delta' && data && typeof data.index === 'number') {
          const entry = indexes.get(identity(scope, data.index))
          if (entry && typeof data.arguments === 'string') {
            entry.arguments += data.arguments
            entry.activity.input = input(entry.arguments)
          }
        } else if (kind === 'tool_call_complete' && data) {
          const complete = object(data.tool_call)
          if (complete) {
            const entry = call(string(complete.id), string(complete.name), turn, trace.recorded_at)
            if (entry && typeof complete.arguments === 'string') {
              entry.arguments = complete.arguments
              entry.activity.input = input(complete.arguments)
            }
          }
        } else if (
          [
            'text_delta',
            'text_delta_with_metadata',
            'refusal_delta',
            'refusal_delta_with_index',
            'done',
            'response_terminal',
            'stream_error',
            'unexpected_eof',
          ].includes(string(kind))
        )
          stopThinking()
      } else if (trace.stage === 'canonical_terminal_response' || trace.stage === 'response_after_hook') {
        const content = blocks(payload)
        if (!ordinaryThoughts.has(scope)) {
          thought(scope, content.map(thinkingText).filter(Boolean).join('\n\n'), trace.recorded_at, true)
        }
        stopThinking()
        for (const block of content) {
          if (block.type !== 'tool_use') continue
          const entry = call(string(block.id), string(block.name), turn, trace.recorded_at)
          if (entry && Object.hasOwn(block, 'input')) entry.activity.input = block.input
        }
      } else if (trace.stage === 'platform_tool_call') {
        const entry = call(string(payload.id), string(payload.name), turn, trace.recorded_at)
        if (entry && typeof payload.arguments === 'string') entry.activity.input = input(payload.arguments)
      } else if (trace.stage === 'platform_tool_result') {
        const entry = calls.findLast((entry) => entry.callId === payload.call_id)
        if (entry) {
          entry.activity.live = false
          if (Object.hasOwn(payload, 'content'))
            entry.activity.results.push({
              id: identity(...prefix, 'platform-result', trace.sequence),
              at: trace.recorded_at,
              source: 'platform',
              content: payload.content,
              isError: payload.is_error === true,
            })
        }
      } else if (trace.stage === 'decoded_request') {
        const content = blocks(payload)
        if (Array.isArray(payload.items)) {
          for (const value of payload.items) {
            const item = object(value)
            if (item?.role === 'tool' && typeof item.tool_call_id === 'string' && typeof item.content === 'string') {
              content.push({
                type: 'tool_result',
                tool_use_id: item.tool_call_id,
                content: item.content,
                is_error: item.is_error,
              })
            }
          }
        }
        content.forEach((block, index) => {
          if (
            block.type === 'tool_result' &&
            typeof block.tool_use_id === 'string' &&
            Object.hasOwn(block, 'content')
          ) {
            if (!ordinaryClientResults.has(block.tool_use_id)) {
              clientResults.push({ run, sequence: trace.sequence, at: trace.recorded_at, block, index })
            }
          }
        })
      }
    }

    const ordinaryPlatformResults = new Set<Call>()
    for (const event of events) {
      const payload = object(event.payload)
      if (!payload) continue
      const id = string(payload.tool_id)
      const turn = string(payload.model_turn_id)
      if (event.kind === 'model_thinking_delta') {
        const scope = scopeOf(payload)
        currentTurn = turn
        thought(scope, string(payload.text), event.occurred_at, false)
      } else if (event.kind === 'model_thinking_finished') {
        const activity = thoughts.get(scopeOf(payload))
        if (activity) activity.live = false
      }
      let entry = calls.findLast(
        (entry) => entry.callId === id && (!turn || !entry.modelTurnId || entry.modelTurnId === turn),
      )
      if (event.kind === 'platform_tool_started' || event.kind === 'client_tool_handoff') {
        entry ??= call(id, string(payload.name), turn, event.occurred_at)
        if (entry) {
          if (turn) entry.modelTurnId = turn
          entry.activity.at = Math.min(entry.activity.at, event.occurred_at)
          if (Object.hasOwn(payload, 'input')) entry.activity.input = payload.input
          if (event.kind === 'client_tool_handoff') entry.activity.live = false
        }
      } else if (event.kind === 'platform_tool_finished' && entry) {
        entry.activity.live = false
        if (Object.hasOwn(payload, 'content')) {
          if (!ordinaryPlatformResults.has(entry)) {
            entry.activity.results = []
            ordinaryPlatformResults.add(entry)
          }
          entry.activity.results.push({
            id: identity(...prefix, 'platform-result', event.sequence),
            at: event.occurred_at,
            source: 'platform',
            content: payload.content,
            isError: payload.status === 'failed',
          })
        }
      } else if (event.kind === 'client_tool_result' && Object.hasOwn(payload, 'content')) {
        clientResults.push({
          run,
          sequence: event.sequence,
          at: event.occurred_at,
          block: { tool_use_id: id, content: payload.content, is_error: payload.is_error },
          index: 0,
        })
      }
    }
    activities.sort((a, b) => a.at - b.at)
    output.set(
      `assistant:${run.id}`,
      activities.filter((activity) => activity.kind !== 'thinking' || activity.text.trim()),
    )
    callsByRun.set(run.id, calls)
  }

  // 请求会重放历史：只匹配明确祖先，并去除同一祖先路径的重放；兄弟分支返回独立保留。
  const attached: { runId: string; call: Call; fingerprint: string }[] = []
  const lineages = new Map<string, string[]>()
  const ancestors = (run: RunDetail): string[] => {
    const cached = lineages.get(run.id)
    if (cached) return cached
    const ids: string[] = []
    const visited = new Set([run.id])
    let parent = run.parent_run_id
    while (parent && !visited.has(parent)) {
      visited.add(parent)
      ids.push(parent)
      parent = runsById.get(parent)?.parent_run_id ?? null
    }
    lineages.set(run.id, ids)
    return ids
  }
  clientResults.sort((a, b) => ancestors(a.run).length - ancestors(b.run).length || a.sequence - b.sequence)
  for (const result of clientResults) {
    const lineage = ancestors(result.run)
    let entry: Call | undefined
    for (const id of lineage) {
      entry = callsByRun.get(id)?.findLast((call) => call.callId === result.block.tool_use_id)
      if (entry) break
    }
    if (!entry) continue
    const fingerprint = JSON.stringify([result.block.content, result.block.is_error === true])
    if (
      attached.some(
        (prior) =>
          prior.call === entry &&
          prior.fingerprint === fingerprint &&
          (prior.runId === result.run.id || lineage.includes(prior.runId)),
      )
    )
      continue
    attached.push({ runId: result.run.id, call: entry, fingerprint })
    entry.activity.live = false
    entry.activity.results.push({
      id: identity(detail.interaction.id, result.run.id, 'client-result', result.sequence, result.index),
      at: result.at,
      source: 'client',
      content: result.block.content,
      isError: result.block.is_error === true,
    })
  }
  return output
}
