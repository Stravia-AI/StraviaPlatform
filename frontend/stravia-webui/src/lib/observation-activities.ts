import { payloadRecord, payloadString } from './observation-payload'
import type { InteractionDetail, LiveContentBlock, RunDetail } from './types/observation'

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
interface Call {
  callId: string
  modelTurnId: string
  activity: ToolActivity
}
interface ClientResult {
  run: RunDetail
  sequence: number
  at: number
  block: ObjectValue
  index: number
}

const object = payloadRecord
function string(value: unknown): string {
  return payloadString(value) ?? ''
}
function identity(...parts: (string | number)[]): string {
  return JSON.stringify(parts)
}
/** 从普通观察事件派生活动，不混入已交付的对话正文。 */
function durableActivities(detail: InteractionDetail): Map<string, ObservationActivity[]> {
  const output = new Map<string, ObservationActivity[]>()
  const callsByRun = new Map<string, Call[]>()
  const runsById = new Map(detail.runs.map((run) => [run.id, run]))
  const clientResults: ClientResult[] = []

  for (const run of detail.runs) {
    const activities: ObservationActivity[] = []
    const calls: Call[] = []
    const thoughts = new Map<string, ThinkingActivity>()
    const prefix = [detail.interaction.id, run.id]
    const events = [
      ...new Map(
        run.events.filter((event) => event.run_id === run.id).map((event) => [event.sequence, event]),
      ).values(),
    ].sort((a, b) => a.sequence - b.sequence)
    const running =
      run.status === 'running' &&
      !events.some(
        (event) =>
          event.kind === 'run_finished' ||
          (event.kind === 'run_state_changed' && object(event.payload).status !== 'running'),
      )
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
        existing = { callId: id, modelTurnId, activity }
        calls.push(existing)
        activities.push(activity)
      }
      existing.activity.name = name
      return existing
    }
    for (const event of events) {
      const payload = object(event.payload)
      const id = string(payload.tool_id)
      const turn = string(payload.model_turn_id)
      // ADR-0062：model_thinking 按 Canonical Item 一行（含取消/失败时的部分正文），落库即终态；
      // live 只来自尚未落盘的易失块。条目身份与易失通道共用 block_id，迁移旧行退回 item/作用域。
      if (event.kind === 'model_thinking') {
        const text = string(payload.text)
        const key =
          string(payload.block_id) ||
          string(payload.item) ||
          identity(string(payload.model_turn_id), string(payload.attempt_id))
        let activity = thoughts.get(key)
        if (!activity && text) {
          activity = {
            kind: 'thinking',
            id: identity(...prefix, 'thinking', key),
            at: event.occurred_at,
            text: '',
            live: false,
          }
          thoughts.set(key, activity)
          activities.push(activity)
        }
        if (activity && text) activity.text = text
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

export function observationConversationActivities(
  detail: InteractionDetail,
  blocks: LiveContentBlock[] = [],
  durable = durableActivities(detail),
): Map<string, ObservationActivity[]> {
  const thoughts = blocks.filter(
    (block) => block.interaction_id === detail.interaction.id && block.kind === 'model_thinking_delta',
  )
  if (!thoughts.length) return durable
  const output = new Map(durable)
  const updated = new Set<string>()
  for (const block of thoughts) {
    const key = `assistant:${block.run_id}`
    if (!updated.has(key)) {
      output.set(key, [...(durable.get(key) ?? [])])
      updated.add(key)
    }
    const activities = output.get(key)!
    // 易失块与持久事件共用 block_id（每个 Canonical Item 一条）：已落库的正文是权威值，不重复拼接。
    const id = identity(detail.interaction.id, block.run_id, 'thinking', block.block_id)
    const index = activities.findIndex((activity) => activity.id === id)
    const prior = activities[index]
    if (prior?.kind === 'thinking') continue
    activities.push({ kind: 'thinking', id, at: block.occurred_at, text: block.text, live: true })
  }
  return output
}
