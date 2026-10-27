import type { ObservationActivity } from './observation-activities'
import { payloadRecord, payloadString } from './observation-payload'
import type { InteractionDetail } from './types/observation'

export type HitKind = 'created' | 'reused'

/** 一个高亮目标上的命中：新增优先于复用，规则按 ID 去重。 */
export interface HitMark {
  kind: HitKind
  ruleIds: readonly string[]
}

export interface UnresolvedHit {
  /** 最早报告该命中的 Run，用于跳到诊断标签。 */
  runId: string
  mark: HitMark
}

export interface CredentialHitResolution {
  /** 键为对话里的用户消息 ID。 */
  userMessages: ReadonlyMap<string, HitMark>
  /** 键为工具调用 ID；命中在工具参数里。 */
  toolCalls: ReadonlyMap<string, HitMark>
  /** 键为工具调用 ID；命中在工具结果里。 */
  toolResults: ReadonlyMap<string, HitMark>
  /** 解析不到块的命中：历史重放、system、其他来源、已不在本 Interaction 的工具调用。 */
  unresolved: readonly UnresolvedHit[]
  /** 新增 / 复用的处数，含 `unresolved`。 */
  created: number
  reused: number
}

export interface HitResolutionContext {
  /** Run ID → 该 Run 带来的用户输入在对话里的消息 ID；工具续跑等没有新用户输入的 Run 不在其中。 */
  userMessageIdByRun: ReadonlyMap<string, string>
  /** 对话里展示的活动：工具调用块与结果块按它们的 `callId` 定位。 */
  activities: Iterable<ObservationActivity>
}

/** 请求记录检查器的凭据命中高亮：存在即处于高亮状态。 */
export interface CredentialHitHighlight {
  ruleNames: ReadonlyMap<string, string>
  onclear: () => void
}

interface Accumulator {
  kind: HitKind
  rules: Set<string>
}

function add(targets: Map<string, Accumulator>, key: string, kind: HitKind, ruleId: string): void {
  const current = targets.get(key)
  if (!current) targets.set(key, { kind, rules: new Set([ruleId]) })
  else {
    if (kind === 'created') current.kind = 'created'
    current.rules.add(ruleId)
  }
}

function mark(target: Accumulator): HitMark {
  return { kind: target.kind, ruleIds: [...target.rules].sort() }
}

function marks(targets: Map<string, Accumulator>): Map<string, HitMark> {
  return new Map([...targets].map(([key, target]) => [key, mark(target)]))
}

const EVENT_KINDS: Record<string, HitKind> = {
  credential_mappings_created: 'created',
  credential_mappings_reused: 'reused',
}

/**
 * 从已加载的 `credential_mappings_*` 事件里解析块级定位。没有 `locators` 的旧事件不产生高亮：
 * 它们只知道规则与来源类型，指不到具体块。
 */
export function resolveCredentialHits(
  detail: InteractionDetail,
  context: HitResolutionContext,
): CredentialHitResolution {
  const userMessages = new Map<string, Accumulator>()
  const toolCalls = new Map<string, Accumulator>()
  const toolResults = new Map<string, Accumulator>()
  const unresolved = new Map<string, Accumulator & { runId: string }>()
  const toolCallIds = new Set<string>()
  const toolResultIds = new Set<string>()
  for (const activity of context.activities) {
    if (activity.kind !== 'tool') continue
    toolCallIds.add(activity.callId)
    if (activity.results.length > 0) toolResultIds.add(activity.callId)
  }

  // 工具续跑没有新的用户输入，没有自己的消息；它重放的「最新用户输入」就是此前最近的那条用户消息。
  let latestMessageId: string | undefined
  for (const run of detail.runs.toSorted((a, b) => a.started_at - b.started_at)) {
    latestMessageId = context.userMessageIdByRun.get(run.id) ?? latestMessageId
    const events = run.events
      .filter((event) => event.run_id === run.id && event.kind in EVENT_KINDS)
      .toSorted((a, b) => a.sequence - b.sequence)
    for (const event of events) {
      const kind = EVENT_KINDS[event.kind]
      const discoveries = payloadRecord(event.payload).discoveries
      if (!Array.isArray(discoveries)) continue
      for (const discovery of discoveries) {
        const locators = payloadRecord(discovery).locators
        if (!Array.isArray(locators)) continue
        for (const entry of locators) {
          const locator = payloadRecord(entry)
          const ruleId = payloadString(locator.rule_id)
          const source = payloadString(locator.source_type)
          if (!ruleId || !source) continue
          const toolId = payloadString(locator.tool_id)
          const latestInput = typeof locator.latest_input === 'boolean' ? locator.latest_input : undefined
          if (toolId && toolCallIds.has(toolId) && source === 'tool_arguments') {
            add(toolCalls, toolId, kind, ruleId)
          } else if (toolId && toolResultIds.has(toolId) && source === 'tool_result') {
            add(toolResults, toolId, kind, ruleId)
          } else if (latestMessageId && source === 'user_message' && latestInput === true) {
            add(userMessages, latestMessageId, kind, ruleId)
          } else {
            // 历史会被后续每个 Run 重放：同一来源只算一处，指向最早报告它的 Run。
            const key = JSON.stringify([source, toolId ?? null, latestInput ?? null, ruleId])
            const current = unresolved.get(key)
            if (!current) unresolved.set(key, { kind, rules: new Set([ruleId]), runId: run.id })
            else if (kind === 'created') current.kind = 'created'
          }
        }
      }
    }
  }

  const unresolvedHits = [...unresolved.values()].map((hit) => ({ runId: hit.runId, mark: mark(hit) }))
  const all = [
    ...marks(userMessages).values(),
    ...marks(toolCalls).values(),
    ...marks(toolResults).values(),
    ...unresolvedHits.map((hit) => hit.mark),
  ]
  const created = all.filter((hit) => hit.kind === 'created').length
  return {
    userMessages: marks(userMessages),
    toolCalls: marks(toolCalls),
    toolResults: marks(toolResults),
    unresolved: unresolvedHits,
    created,
    reused: all.length - created,
  }
}
