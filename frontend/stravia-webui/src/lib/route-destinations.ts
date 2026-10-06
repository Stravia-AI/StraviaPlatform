import * as m from '$lib/paraglide/messages.js'
import { priorityLanes } from '$lib/components/route-targets-form'
import type { Provider, Route, RouteSelectionStrategy, Target } from '$lib/types'

export type TargetUnavailableReason = 'provider_disabled' | 'credential_invalid' | 'allowance_suspended'

export type PreferredDestination =
  | { kind: 'single'; modelName: string; providerName: string }
  | { kind: 'multiple'; count: number; strategy: RouteSelectionStrategy }

/**
 * 模型列表「请求目标」列的结构化摘要。
 *
 * 只使用已加载的 Provider 启用状态、凭据失效与额度暂停判断可用性；冷却与半开探测需要逐个模型轮询，
 * 留给模型编辑器展示。层按已启用 Target 的 priority 降序编号，层数不设上限。
 */
export interface RouteDestinationSummary {
  /** 首选层：优先级最高且含可用目标的层；全部不可用时为 null。 */
  preferred: PreferredDestination | null
  /** 首选层之前整层不可用的层数；按定义总是从第 1 层开始的连续区间。 */
  skippedLayers: number
  /** 首选层之后仍含可用目标的层数及其可用目标数。 */
  fallbackLayers: number
  fallbackTargets: number
  /** 首选层及之后的不可用目标数；被跳过各层的目标已由 skippedLayers 表达。 */
  unavailableTargets: number
  /** 没有首选层时按原因统计全部已启用目标；每个目标只计入优先级最高的一个原因。 */
  unavailableReasons: Record<TargetUnavailableReason, number>
}

function unavailableReason(
  target: Target,
  providers: ReadonlyMap<string, Provider>,
): TargetUnavailableReason | 'provider_missing' | null {
  const provider = providers.get(target.provider_id)
  // 两个查询之间 Provider 被删除时，目标已无法路由，但没有可向用户陈述的原因。
  if (!provider) return 'provider_missing'
  if (!provider.is_enabled) return 'provider_disabled'
  if (provider.credential_status === 'invalid') return 'credential_invalid'
  if (provider.allowance_suspension) return 'allowance_suspended'
  return null
}

export function summarizeRouteDestinations(route: Route, providers: readonly Provider[]): RouteDestinationSummary {
  const providerById = new Map(providers.map((provider) => [provider.id, provider]))
  const lanes = priorityLanes(route.targets).map((lane) =>
    lane.targets.map((target) => ({ target, reason: unavailableReason(target, providerById) })),
  )
  const unavailableReasons: Record<TargetUnavailableReason, number> = {
    provider_disabled: 0,
    credential_invalid: 0,
    allowance_suspended: 0,
  }
  const preferredIndex = lanes.findIndex((lane) => lane.some(({ reason }) => reason === null))

  if (preferredIndex === -1) {
    for (const { reason } of lanes.flat()) if (reason && reason !== 'provider_missing') unavailableReasons[reason] += 1
    return {
      preferred: null,
      skippedLayers: 0,
      fallbackLayers: 0,
      fallbackTargets: 0,
      unavailableTargets: 0,
      unavailableReasons,
    }
  }

  const preferredTargets = lanes[preferredIndex].filter(({ reason }) => reason === null).map(({ target }) => target)
  let fallbackLayers = 0
  let fallbackTargets = 0
  let unavailableTargets = 0
  for (const [index, lane] of lanes.entries()) {
    if (index < preferredIndex) continue
    const available = lane.filter(({ reason }) => reason === null).length
    unavailableTargets += lane.length - available
    if (index > preferredIndex && available > 0) {
      fallbackLayers += 1
      fallbackTargets += available
    }
  }

  const [first] = preferredTargets
  return {
    preferred:
      preferredTargets.length === 1
        ? {
            kind: 'single',
            modelName: first.model_name?.trim() || first.model,
            providerName: providerById.get(first.provider_id)?.name ?? first.provider_id,
          }
        : { kind: 'multiple', count: preferredTargets.length, strategy: route.balance },
    skippedLayers: preferredIndex,
    fallbackLayers,
    fallbackTargets,
    unavailableTargets,
    unavailableReasons,
  }
}

/** 排序用严重度：数值越大越需要处理。 */
export function routeDestinationSeverity(summary: RouteDestinationSummary): number {
  if (!summary.preferred) return 3
  if (summary.skippedLayers > 0) return 2
  if (summary.unavailableTargets > 0) return 1
  return 0
}

export interface RouteDestinationText {
  primary: string
  details: { text: string; tone: 'warning' | 'muted' }[]
}

function strategyLabel(strategy: RouteSelectionStrategy): string {
  return strategy === 'latency_preference' ? m.model_editor_latency_preference() : m.model_editor_traffic_equalization()
}

export function formatRouteDestinations(summary: RouteDestinationSummary): RouteDestinationText {
  const { preferred } = summary
  if (!preferred) {
    const { credential_invalid, provider_disabled, allowance_suspended } = summary.unavailableReasons
    const details: RouteDestinationText['details'] = []
    if (credential_invalid > 0) {
      details.push({ text: m.models_destinations_credential_invalid({ count: credential_invalid }), tone: 'muted' })
    }
    if (provider_disabled > 0) {
      details.push({ text: m.models_destinations_provider_disabled({ count: provider_disabled }), tone: 'muted' })
    }
    if (allowance_suspended > 0) {
      details.push({ text: m.models_destinations_allowance_suspended({ count: allowance_suspended }), tone: 'muted' })
    }
    return { primary: m.models_destinations_none_available(), details }
  }

  const details: RouteDestinationText['details'] = []
  if (summary.skippedLayers === 1) {
    details.push({ text: m.models_destinations_first_layer_unavailable(), tone: 'warning' })
  } else if (summary.skippedLayers > 1) {
    details.push({ text: m.models_destinations_layers_unavailable({ last: summary.skippedLayers }), tone: 'warning' })
  }
  if (summary.fallbackLayers > 0) {
    details.push({
      text: m.models_destinations_fallback({ layers: summary.fallbackLayers, targets: summary.fallbackTargets }),
      tone: 'muted',
    })
  }
  if (summary.unavailableTargets > 0) {
    details.push({ text: m.models_destinations_unavailable({ count: summary.unavailableTargets }), tone: 'muted' })
  }
  return {
    primary:
      preferred.kind === 'single'
        ? `${preferred.modelName} · ${preferred.providerName}`
        : m.models_destinations_multiple({ count: preferred.count, strategy: strategyLabel(preferred.strategy) }),
    details,
  }
}
