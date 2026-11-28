import {
  effortCandidates,
  thinkingControlWritable,
  type ThinkingControlContext,
} from '#lib/thinking-control.js'
import type {
  CreateTarget,
  Provider,
  ProviderModelSummary,
  Route,
  RoutePurpose,
  TargetConfigMode,
  ThinkingLevel,
  ThinkingLevelMapping,
} from '#lib/types.js'

export interface RouteTargetForm {
  key: string
  id?: string
  providerId: string
  model: string
  /** Provider Model 快照保存的展示名（库存未就绪前的回退名）。 */
  modelName?: string | null
  enabled: boolean
  priority: number
  firstTokenTimeoutSeconds: number
  targetRetryBudget: number
  targetCooldownSeconds: number
  /** 同层同模型组内的配置归属：`shared` 跟随模型组共享配置，`custom` 单账号独立覆盖。 */
  configMode: TargetConfigMode
  inventory: ProviderModelSummary[]
  custom: boolean
  persisted: boolean
  loading: boolean
  validationError: string
  thinkingLevelMap: ThinkingLevelMapping[]
}

export type RouteTargetsValidationError =
  | 'invalid-priority'
  | 'invalid-first-token-timeout'
  | 'invalid-retry-budget'
  | 'invalid-cooldown'
  | 'incomplete-target'
  | 'no-enabled-target'

export interface RouteTargetLane<T extends PriorityLaneMember = RouteTargetForm> {
  priority: number
  targets: T[]
}

/** 编辑器表单与已保存 Target 共用的分层依据：仅已启用目标参与，同一 priority 为一层。 */
export interface PriorityLaneMember {
  enabled: boolean
  priority: number
}

export type RouteTargetInsertion =
  | { position: 'top' }
  | { position: 'bottom' }
  | { position: 'between'; upperPriority: number; lowerPriority: number }

export type RouteTargetGroupScope = { enabled: true; priority: number } | { enabled: false }

export interface TargetGroupConfig {
  firstTokenTimeoutSeconds: number
  targetRetryBudget: number
  targetCooldownSeconds: number
  thinkingLevelMap: ThinkingLevelMapping[]
}

export interface RouteTargetModelGroup {
  model: string
  scope: RouteTargetGroupScope
  targets: RouteTargetForm[]
  sharedTargets: RouteTargetForm[]
  customTargets: RouteTargetForm[]
  /** 优先取首个 `shared` 成员配置作为共享基线；若全组均为 `custom`，取首个成员用于预览与初始编辑。 */
  sharedConfig: TargetGroupConfig
}

export interface GroupThinkingCompatibilityIssue {
  level: ThinkingLevel
  effort: string
  incompatibleMembers: string[]
}

export interface GroupEffortSupportSummary {
  /** 当组内成员均未声明推理档位时为 `null`（允许自由输入）；否则为所有成员共同支持的交集。 */
  common: string[] | null
  /** 相比同组其他成员缺少某些档位的成员差异列表。 */
  differences: { label: string; missing: string[] }[]
}

export type SplitMergeStepAction = 'split-up' | 'merge-up' | 'split-down' | 'merge-down'

/** 筛选包含支持当前路由用途的可用模型的供应商；不按启用状态过滤，允许为已停用供应商预配目标。 */
export function providersForRoutePurpose(providers: readonly Provider[], purpose: RoutePurpose | null): Provider[] {
  if (!purpose) return []
  return providers.filter((provider) => provider.model_inventory?.route_purposes.includes(purpose) ?? false)
}

export const THINKING_LEVELS: ThinkingLevel[] = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']

export function routeSupportedThinkingLevels(targets: RouteTargetForm[]): ThinkingLevel[] {
  const enabled = targets.filter((target) => target.enabled)
  if (enabled.length === 0) return []
  return THINKING_LEVELS.filter((level) =>
    enabled.some((target) =>
      target.thinkingLevelMap.some((row) => row.level === level && row.control.type !== 'hidden'),
    ),
  )
}

export interface RouteTargetInsertionPlan {
  priority: number
  shift?: { direction: 'up' | 'down'; boundary: number }
}

const i32Minimum = -2_147_483_648
const i32Maximum = 2_147_483_647

function cloneThinkingMap(rows: readonly ThinkingLevelMapping[]): ThinkingLevelMapping[] {
  return rows.map((row) => ({ ...row, control: { ...row.control } }))
}

export function extractTargetGroupConfig(target: RouteTargetForm): TargetGroupConfig {
  return {
    firstTokenTimeoutSeconds: target.firstTokenTimeoutSeconds,
    targetRetryBudget: target.targetRetryBudget,
    targetCooldownSeconds: target.targetCooldownSeconds,
    thinkingLevelMap: cloneThinkingMap(target.thinkingLevelMap),
  }
}

export function applyTargetGroupConfig(target: RouteTargetForm, config: TargetGroupConfig): void {
  target.firstTokenTimeoutSeconds = config.firstTokenTimeoutSeconds
  target.targetRetryBudget = config.targetRetryBudget
  target.targetCooldownSeconds = config.targetCooldownSeconds
  if (config.thinkingLevelMap.length > 0) {
    target.thinkingLevelMap = cloneThinkingMap(config.thinkingLevelMap)
  }
}

function matchesGroupScope(target: RouteTargetForm, scope: RouteTargetGroupScope): boolean {
  if (!scope.enabled) return !target.enabled
  return target.enabled && target.priority === scope.priority
}

function findSharedPeerInScope(
  targets: readonly RouteTargetForm[],
  scope: RouteTargetGroupScope,
  model: string,
  excludeKeys?: ReadonlySet<string>,
): RouteTargetForm | undefined {
  const normalized = model.trim()
  if (!normalized) return undefined
  return targets.find(
    (candidate) =>
      !excludeKeys?.has(candidate.key) &&
      matchesGroupScope(candidate, scope) &&
      candidate.model.trim() === normalized &&
      candidate.configMode === 'shared',
  )
}

function nextTargetKey(targets: RouteTargetForm[]): string {
  const next =
    targets.reduce((maximum, target) => {
      const match = /^target-(\d+)$/.exec(target.key)
      return Math.max(maximum, match ? Number(match[1]) : 0)
    }, 0) + 1
  return `target-${next}`
}

function targetKeyOrder(target: RouteTargetForm): number {
  return Number(/^target-(\d+)$/.exec(target.key)?.[1] ?? Number.MAX_SAFE_INTEGER)
}

export function createRouteTarget(targets: RouteTargetForm[], values: Partial<RouteTargetForm> = {}): RouteTargetForm {
  return {
    key: nextTargetKey(targets),
    providerId: '',
    model: '',
    modelName: null,
    enabled: false,
    priority: 0,
    firstTokenTimeoutSeconds: 120,
    targetRetryBudget: 5,
    targetCooldownSeconds: 120,
    configMode: 'shared',
    inventory: [],
    custom: false,
    persisted: false,
    loading: false,
    validationError: '',
    thinkingLevelMap: [],
    ...values,
  }
}

export function createRouteTargetForms(
  route: Route | undefined,
  initialProviderId: string,
  initialModelId: string,
): RouteTargetForm[] {
  const targets: RouteTargetForm[] = []
  if (route?.targets.length) {
    for (const target of route.targets) {
      targets.push(
        createRouteTarget(targets, {
          id: target.id,
          providerId: target.provider_id,
          model: target.model,
          modelName: target.model_name ?? null,
          enabled: target.enabled ?? true,
          priority: target.priority,
          firstTokenTimeoutSeconds: millisecondsToSeconds(target.first_token_timeout_ms),
          targetRetryBudget: target.target_retry_budget,
          targetCooldownSeconds: millisecondsToSeconds(target.target_cooldown_ms),
          configMode: target.config_mode ?? 'shared',
          persisted: true,
          thinkingLevelMap: cloneThinkingMap(target.thinking_level_map ?? []),
        }),
      )
    }
    return targets
  }
  if (!initialProviderId || !initialModelId.trim()) return targets
  return [createRouteTarget(targets, { providerId: initialProviderId, model: initialModelId, enabled: true })]
}

export function addRouteTarget(targets: RouteTargetForm[]): RouteTargetForm {
  const target = createRouteTarget(targets)
  targets.push(target)
  return target
}

export function removeRouteTarget(targets: RouteTargetForm[], index: number): boolean {
  const target = targets[index]
  if (!target) return false
  if (target.enabled && targets.filter((candidate) => candidate.enabled).length === 1) return false
  targets.splice(index, 1)
  return true
}

function validInteger(value: number, minimum = 0, maximum?: number): boolean {
  const number = Number(value)
  return Number.isInteger(number) && number >= minimum && (maximum === undefined || number <= maximum)
}

function validSeconds(value: number): boolean {
  const number = Number(value)
  const milliseconds = Math.round(number * 1000)
  return (
    Number.isFinite(number) &&
    number >= 0 &&
    Number.isSafeInteger(milliseconds) &&
    Math.abs(number * 1000 - milliseconds) < 1e-6
  )
}

/** 表单就地提示越界约束时与保存校验使用同一规则。 */
export const isValidTargetSeconds = validSeconds
export const isValidTargetRetryBudget = (value: number): boolean => validInteger(value)

function completeTarget(target: RouteTargetForm): boolean {
  return Boolean(target.providerId && target.model.trim())
}

export function millisecondsToSeconds(milliseconds: number): number {
  return milliseconds / 1000
}

export function secondsToMilliseconds(seconds: number): number | undefined {
  if (!validSeconds(seconds)) return undefined
  return Math.round(Number(seconds) * 1000)
}

export function priorityLanes<T extends PriorityLaneMember>(targets: readonly T[]): RouteTargetLane<T>[] {
  const lanes = new Map<number, T[]>()
  for (const target of targets) {
    if (!target.enabled) continue
    const lane = lanes.get(target.priority)
    if (lane) lane.push(target)
    else lanes.set(target.priority, [target])
  }
  return [...lanes.entries()]
    .sort(([left], [right]) => right - left)
    .map(([priority, laneTargets]) => ({ priority, targets: laneTargets }))
}

/** 同一优先级层（或停用区）内按精确上游模型 ID 聚合为模型组，保留首次出现顺序。 */
export function groupLaneTargetsByModel(
  targets: readonly RouteTargetForm[],
  scope: RouteTargetGroupScope,
): RouteTargetModelGroup[] {
  const groups = new Map<string, RouteTargetForm[]>()
  for (const target of targets) {
    const modelKey = target.model.trim()
    const list = groups.get(modelKey)
    if (list) list.push(target)
    else groups.set(modelKey, [target])
  }
  return [...groups.entries()].map(([model, members]) => {
    const sharedTargets = members.filter((member) => member.configMode === 'shared')
    const customTargets = members.filter((member) => member.configMode === 'custom')
    const anchor = sharedTargets[0] ?? members[0]
    return {
      model,
      scope,
      targets: members,
      sharedTargets,
      customTargets,
      sharedConfig: extractTargetGroupConfig(anchor),
    }
  })
}

export function moveRouteTargetToLane(targets: RouteTargetForm[], key: string, priority: number): boolean {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target || !completeTarget(target) || !validInteger(priority, i32Minimum, i32Maximum)) return false
  target.enabled = true
  target.priority = priority
  return true
}

/** 单目标移入指定层；若处于 `shared` 且目标层已存在同模型共享组，则自动继承目标组配置。 */
export function moveTargetToLaneWithInheritance(
  targets: RouteTargetForm[],
  key: string,
  priority: number,
): { ok: boolean; inheritedFromGroup: boolean } {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target || !completeTarget(target) || !validInteger(priority, i32Minimum, i32Maximum)) {
    return { ok: false, inheritedFromGroup: false }
  }
  const alreadyThere = target.enabled && target.priority === priority
  const peer =
    !alreadyThere && target.configMode === 'shared'
      ? findSharedPeerInScope(targets, { enabled: true, priority }, target.model, new Set([key]))
      : undefined
  target.enabled = true
  target.priority = priority
  if (peer) {
    applyTargetGroupConfig(target, extractTargetGroupConfig(peer))
    return { ok: true, inheritedFromGroup: true }
  }
  return { ok: true, inheritedFromGroup: false }
}

function planKeysInsertion(
  targets: RouteTargetForm[],
  movingKeys: ReadonlySet<string>,
  insertion: RouteTargetInsertion,
): RouteTargetInsertionPlan | undefined {
  const moving = targets.filter((candidate) => movingKeys.has(candidate.key))
  if (moving.length === 0 || moving.some((candidate) => !completeTarget(candidate))) return undefined
  const enabledPriorities = targets
    .filter((candidate) => candidate.enabled && !movingKeys.has(candidate.key))
    .map((candidate) => candidate.priority)
  if (enabledPriorities.length === 0) {
    return { priority: moving[0].priority }
  }

  if (insertion.position === 'top') {
    const maximum = Math.max(...enabledPriorities)
    return maximum === i32Maximum ? undefined : { priority: maximum + 1 }
  }
  if (insertion.position === 'bottom') {
    const minimum = Math.min(...enabledPriorities)
    return minimum === i32Minimum ? undefined : { priority: minimum - 1 }
  }

  const { upperPriority, lowerPriority } = insertion
  if (
    !validInteger(upperPriority, i32Minimum, i32Maximum) ||
    !validInteger(lowerPriority, i32Minimum, i32Maximum) ||
    upperPriority <= lowerPriority
  ) {
    return undefined
  }
  if (upperPriority - lowerPriority > 1) {
    return { priority: lowerPriority + Math.floor((upperPriority - lowerPriority) / 2) }
  }

  const minimum = Math.min(...enabledPriorities)
  if (minimum > i32Minimum) {
    return { priority: lowerPriority, shift: { direction: 'down', boundary: lowerPriority } }
  }
  const maximum = Math.max(...enabledPriorities)
  if (maximum < i32Maximum) {
    return { priority: upperPriority, shift: { direction: 'up', boundary: upperPriority } }
  }
  return undefined
}

export function planRouteTargetInsertion(
  targets: RouteTargetForm[],
  key: string,
  insertion: RouteTargetInsertion,
): RouteTargetInsertionPlan | undefined {
  return planKeysInsertion(targets, new Set([key]), insertion)
}

export function moveRouteTargetToInsertion(
  targets: RouteTargetForm[],
  key: string,
  insertion: RouteTargetInsertion,
): boolean {
  return moveModelGroupToInsertion(targets, [key], insertion)
}

/** 整组（或多选目标）插入新优先级层；完整保留组内 `shared` 与 `custom` 配置。 */
export function moveModelGroupToInsertion(
  targets: RouteTargetForm[],
  keys: readonly string[],
  insertion: RouteTargetInsertion,
): boolean {
  const movingSet = new Set(keys)
  const plan = planKeysInsertion(targets, movingSet, insertion)
  if (!plan) return false
  if (plan.shift) {
    for (const candidate of targets) {
      if (!candidate.enabled || movingSet.has(candidate.key)) continue
      if (plan.shift.direction === 'down' && candidate.priority <= plan.shift.boundary) candidate.priority -= 1
      if (plan.shift.direction === 'up' && candidate.priority >= plan.shift.boundary) candidate.priority += 1
    }
  }
  for (const candidate of targets) {
    if (!movingSet.has(candidate.key)) continue
    candidate.enabled = true
    candidate.priority = plan.priority
  }
  return true
}

/** 整组移入既有层；若目标层已存在同模型共享组，则迁入的 `shared` 成员同步目标组共享配置，`custom` 保留独立。 */
export function moveModelGroupToLane(
  targets: RouteTargetForm[],
  keys: readonly string[],
  targetPriority: number,
): { ok: boolean; syncedShared: number } {
  const movingSet = new Set(keys)
  const moving = targets.filter((candidate) => movingSet.has(candidate.key))
  if (
    moving.length === 0 ||
    moving.some((candidate) => !completeTarget(candidate)) ||
    !validInteger(targetPriority, i32Minimum, i32Maximum)
  ) {
    return { ok: false, syncedShared: 0 }
  }
  const destScope: RouteTargetGroupScope = { enabled: true, priority: targetPriority }
  let syncedShared = 0
  for (const candidate of moving) {
    const alreadyThere = candidate.enabled && candidate.priority === targetPriority
    const peer =
      !alreadyThere && candidate.configMode === 'shared'
        ? findSharedPeerInScope(targets, destScope, candidate.model, movingSet)
        : undefined
    candidate.enabled = true
    candidate.priority = targetPriority
    if (peer) {
      applyTargetGroupConfig(candidate, extractTargetGroupConfig(peer))
      syncedShared += 1
    }
  }
  return { ok: true, syncedShared }
}

/** 整层互换两个相邻优先级层的 priority 值。 */
export function swapAdjacentLanes(
  targets: RouteTargetForm[],
  upperPriority: number,
  lowerPriority: number,
): boolean {
  if (upperPriority === lowerPriority) return false
  const upperTargets = targets.filter((candidate) => candidate.enabled && candidate.priority === upperPriority)
  const lowerTargets = targets.filter((candidate) => candidate.enabled && candidate.priority === lowerPriority)
  if (upperTargets.length === 0 || lowerTargets.length === 0) return false
  for (const candidate of upperTargets) candidate.priority = lowerPriority
  for (const candidate of lowerTargets) candidate.priority = upperPriority
  return true
}

/** 将下层全部已启用目标并入上层；若两侧存在同模型共享组，下层 `shared` 成员自动采用上层组配置。 */
export function mergeLaneIntoUpper(
  targets: RouteTargetForm[],
  lowerPriority: number,
  upperPriority: number,
): { moved: number; syncedShared: number } {
  const lowerTargets = targets.filter((candidate) => candidate.enabled && candidate.priority === lowerPriority)
  if (lowerTargets.length === 0 || lowerPriority === upperPriority) return { moved: 0, syncedShared: 0 }
  const lowerKeys = new Set(lowerTargets.map((candidate) => candidate.key))
  const upperScope: RouteTargetGroupScope = { enabled: true, priority: upperPriority }
  let syncedShared = 0
  for (const candidate of lowerTargets) {
    const upperPeer =
      candidate.configMode === 'shared'
        ? findSharedPeerInScope(targets, upperScope, candidate.model, lowerKeys)
        : undefined
    candidate.priority = upperPriority
    if (upperPeer) {
      applyTargetGroupConfig(candidate, extractTargetGroupConfig(upperPeer))
      syncedShared += 1
    }
  }
  return { moved: lowerTargets.length, syncedShared }
}

/**
 * 单目标严格可逆的 Split ↔ Merge 半步上下移动状态机：
 * - 层内有其他成员 → 先拆成独立相邻层（split-up / split-down）；
 * - 独自一层 → 并入相邻层（merge-up / merge-down），并按 `shared` 规则继承目标同模型组配置。
 */
export function stepTargetSplitMerge(
  targets: RouteTargetForm[],
  key: string,
  direction: 'up' | 'down',
): { ok: boolean; action?: SplitMergeStepAction; priority?: number; inheritedFromGroup?: boolean } {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target || !target.enabled || !completeTarget(target)) return { ok: false }
  const lanes = priorityLanes(targets)
  const laneIndex = lanes.findIndex((lane) => lane.priority === target.priority)
  if (laneIndex < 0) return { ok: false }
  const currentLane = lanes[laneIndex]

  if (currentLane.targets.length > 1) {
    const insertion: RouteTargetInsertion =
      direction === 'up'
        ? laneIndex === 0
          ? { position: 'top' }
          : { position: 'between', upperPriority: lanes[laneIndex - 1].priority, lowerPriority: currentLane.priority }
        : laneIndex === lanes.length - 1
          ? { position: 'bottom' }
          : { position: 'between', upperPriority: currentLane.priority, lowerPriority: lanes[laneIndex + 1].priority }
    if (!moveRouteTargetToInsertion(targets, key, insertion)) return { ok: false }
    return {
      ok: true,
      action: direction === 'up' ? 'split-up' : 'split-down',
      priority: target.priority,
      inheritedFromGroup: false,
    }
  }

  const neighborIndex = direction === 'up' ? laneIndex - 1 : laneIndex + 1
  if (neighborIndex < 0 || neighborIndex >= lanes.length) return { ok: false }
  const destinationPriority = lanes[neighborIndex].priority
  const moved = moveTargetToLaneWithInheritance(targets, key, destinationPriority)
  if (!moved.ok) return { ok: false }
  return {
    ok: true,
    action: direction === 'up' ? 'merge-up' : 'merge-down',
    priority: destinationPriority,
    inheritedFromGroup: moved.inheritedFromGroup,
  }
}

/**
 * 整组上下移层状态机：
 * - 若该层还有其他模型组 → 先拆成独立相邻层（split-up / split-down）；
 * - 若该模型组独占当前层 → 与相邻层互换（若相邻层有其他目标）或并入？
 *   按原型变体 C 契约：整组独占层时直接与相邻层整层互换，保证「整组上移/下移一层」符合直觉且严格可逆。
 */
export function stepModelGroup(
  targets: RouteTargetForm[],
  keys: readonly string[],
  direction: 'up' | 'down',
): { ok: boolean; action?: SplitMergeStepAction | 'swap'; priority?: number } {
  const movingSet = new Set(keys)
  const moving = targets.filter((candidate) => movingSet.has(candidate.key))
  if (moving.length === 0 || moving.some((candidate) => !candidate.enabled || !completeTarget(candidate))) {
    return { ok: false }
  }
  const priority = moving[0].priority
  if (moving.some((candidate) => candidate.priority !== priority)) return { ok: false }

  const lanes = priorityLanes(targets)
  const laneIndex = lanes.findIndex((lane) => lane.priority === priority)
  if (laneIndex < 0) return { ok: false }
  const currentLane = lanes[laneIndex]
  const hasOtherModelsInLane = currentLane.targets.some((candidate) => !movingSet.has(candidate.key))

  if (hasOtherModelsInLane) {
    const insertion: RouteTargetInsertion =
      direction === 'up'
        ? laneIndex === 0
          ? { position: 'top' }
          : { position: 'between', upperPriority: lanes[laneIndex - 1].priority, lowerPriority: currentLane.priority }
        : laneIndex === lanes.length - 1
          ? { position: 'bottom' }
          : { position: 'between', upperPriority: currentLane.priority, lowerPriority: lanes[laneIndex + 1].priority }
    if (!moveModelGroupToInsertion(targets, keys, insertion)) return { ok: false }
    return { ok: true, action: direction === 'up' ? 'split-up' : 'split-down', priority: moving[0].priority }
  }

  const neighborIndex = direction === 'up' ? laneIndex - 1 : laneIndex + 1
  if (neighborIndex < 0 || neighborIndex >= lanes.length) return { ok: false }
  const neighborPriority = lanes[neighborIndex].priority
  if (!swapAdjacentLanes(targets, priority, neighborPriority)) return { ok: false }
  return { ok: true, action: 'swap', priority: neighborPriority }
}

export function moveRouteTargetToDock(targets: RouteTargetForm[], key: string): boolean {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return false
  if (!target.enabled) return true
  if (targets.filter((candidate) => candidate.enabled).length === 1) return false
  target.enabled = false
  return true
}

/** 整组停用；如果停用整组会导致没有剩余已启用目标则拒绝。 */
export function moveModelGroupToDock(targets: RouteTargetForm[], keys: readonly string[]): boolean {
  const movingSet = new Set(keys)
  const remainingEnabled = targets.filter((candidate) => candidate.enabled && !movingSet.has(candidate.key)).length
  if (remainingEnabled === 0) return false
  for (const candidate of targets) {
    if (movingSet.has(candidate.key)) candidate.enabled = false
  }
  return true
}

export function reorderRouteTargetBefore(targets: RouteTargetForm[], key: string, beforeKey: string): boolean {
  if (key === beforeKey) return false
  const from = targets.findIndex((target) => target.key === key)
  const before = targets.findIndex((target) => target.key === beforeKey)
  if (from < 0 || before < 0) return false
  const [target] = targets.splice(from, 1)
  const destination = targets.findIndex((candidate) => candidate.key === beforeKey)
  targets.splice(destination, 0, target)
  return true
}

/**
 * 更新某模型组的共享配置：默认仅同步所有 `configMode === 'shared'` 成员，保护已独立配置账号；
 * 当 `overwriteCustom: true` 时同时覆写所有 `custom` 成员并重置为 `shared`。
 */
export function updateModelGroupConfig(
  targets: RouteTargetForm[],
  scope: RouteTargetGroupScope,
  model: string,
  nextConfig: TargetGroupConfig,
  options: { overwriteCustom?: boolean } = {},
): { updatedCount: number; overwrittenCustomCount: number } {
  const normalized = model.trim()
  const overwriteCustom = options.overwriteCustom ?? false
  let updatedCount = 0
  let overwrittenCustomCount = 0
  for (const candidate of targets) {
    if (!matchesGroupScope(candidate, scope) || candidate.model.trim() !== normalized) continue
    if (candidate.configMode === 'custom') {
      if (!overwriteCustom) continue
      candidate.configMode = 'shared'
      overwrittenCustomCount += 1
    }
    applyTargetGroupConfig(candidate, nextConfig)
    updatedCount += 1
  }
  return { updatedCount, overwrittenCustomCount }
}

/** 切换单个目标的配置归属；切回 `shared` 时立即从同组其他 `shared` 成员同步配置。 */
export function setTargetConfigMode(
  targets: RouteTargetForm[],
  key: string,
  mode: TargetConfigMode,
): { ok: boolean; syncedFromGroup: boolean } {
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return { ok: false, syncedFromGroup: false }
  if (target.configMode === mode) return { ok: true, syncedFromGroup: false }
  target.configMode = mode
  if (mode === 'shared') {
    const scope: RouteTargetGroupScope = target.enabled
      ? { enabled: true, priority: target.priority }
      : { enabled: false }
    const peer = findSharedPeerInScope(targets, scope, target.model, new Set([key]))
    if (peer) {
      applyTargetGroupConfig(target, extractTargetGroupConfig(peer))
      return { ok: true, syncedFromGroup: true }
    }
  }
  return { ok: true, syncedFromGroup: false }
}

/** 为新加入某层同模型组的账号继承现有共享组配置（若有）。 */
export function inheritGroupConfigForNewTarget(
  targets: readonly RouteTargetForm[],
  draft: RouteTargetForm,
  scope: RouteTargetGroupScope,
): boolean {
  const peer = findSharedPeerInScope(targets, scope, draft.model, new Set([draft.key]))
  if (!peer) return false
  draft.configMode = 'shared'
  applyTargetGroupConfig(draft, extractTargetGroupConfig(peer))
  return true
}

/** 结构化摘要（与文案/语言解耦），供模型组头紧凑展示。 */
export function summarizeTargetGroupConfig(config: TargetGroupConfig): {
  timeoutSeconds: number
  retryBudget: number
  cooldownSeconds: number
  enabledThinkingLevels: ThinkingLevel[]
} {
  const enabledThinkingLevels = THINKING_LEVELS.filter((level) =>
    config.thinkingLevelMap.some((row) => row.level === level && row.control.type !== 'hidden'),
  )
  return {
    timeoutSeconds: config.firstTokenTimeoutSeconds,
    retryBudget: config.targetRetryBudget,
    cooldownSeconds: config.targetCooldownSeconds,
    enabledThinkingLevels,
  }
}

/** 实时校验组配置的 7 档思考映射是否被受影响组内全部成员的上游能力所支持。 */
export function checkGroupThinkingCompatibility(
  members: readonly { label: string; context: ThinkingControlContext }[],
  thinkingLevelMap: readonly ThinkingLevelMapping[],
): GroupThinkingCompatibilityIssue[] {
  const issues: GroupThinkingCompatibilityIssue[] = []
  for (const row of thinkingLevelMap) {
    if (row.control.type === 'hidden') continue
    const incompatibleMembers = members
      .filter((member) => !thinkingControlWritable(row.control, member.context))
      .map((member) => member.label)
    if (incompatibleMembers.length > 0) {
      issues.push({
        level: row.level,
        effort: row.control.value,
        incompatibleMembers,
      })
    }
  }
  return issues
}

/** 计算组内所有受影响成员共同支持的推理档位交集，以及各成员相较同组并集缺失的档位。 */
export function commonGroupEffortSupport(
  members: readonly { label: string; context: ThinkingControlContext }[],
): GroupEffortSupportSummary {
  const declaredList = members.map((member) => ({
    label: member.label,
    candidates: effortCandidates(member.context),
  }))
  const constrained = declaredList.filter((item): item is { label: string; candidates: string[] } => item.candidates !== null)
  if (constrained.length === 0) return { common: null, differences: [] }

  const union: string[] = []
  const unionLower = new Set<string>()
  for (const item of constrained) {
    for (const effort of item.candidates) {
      const key = effort.toLowerCase()
      if (!unionLower.has(key)) {
        unionLower.add(key)
        union.push(effort)
      }
    }
  }
  const common = union.filter((effort) => {
    const target = effort.toLowerCase()
    return constrained.every((item) => item.candidates.some((candidate) => candidate.toLowerCase() === target))
  })
  const differences: { label: string; missing: string[] }[] = []
  for (const item of constrained) {
    const missing = union.filter((effort) => {
      const target = effort.toLowerCase()
      return !item.candidates.some((candidate) => candidate.toLowerCase() === target)
    })
    if (missing.length > 0) {
      differences.push({ label: item.label, missing })
    }
  }
  return { common, differences }
}

function hasSharedGroupPeerWithThinkingMap(targets: readonly RouteTargetForm[], target: RouteTargetForm): boolean {
  if (target.configMode !== 'shared') return false
  const scope: RouteTargetGroupScope = target.enabled
    ? { enabled: true, priority: target.priority }
    : { enabled: false }
  const peer = findSharedPeerInScope(targets, scope, target.model, new Set([target.key]))
  return Boolean(peer && peer.thinkingLevelMap.length > 0)
}

export function buildRouteTargets(targets: RouteTargetForm[]): {
  targets: CreateTarget[]
  error?: RouteTargetsValidationError
} {
  if (targets.some((target) => !validInteger(target.priority, i32Minimum, i32Maximum)))
    return { targets: [], error: 'invalid-priority' }
  if (targets.some((target) => !validSeconds(target.firstTokenTimeoutSeconds)))
    return { targets: [], error: 'invalid-first-token-timeout' }
  if (targets.some((target) => !validInteger(target.targetRetryBudget)))
    return { targets: [], error: 'invalid-retry-budget' }
  if (targets.some((target) => !validSeconds(target.targetCooldownSeconds)))
    return { targets: [], error: 'invalid-cooldown' }
  if (!targets.some((target) => target.enabled)) return { targets: [], error: 'no-enabled-target' }

  const cleanTargets = [...targets]
    .sort((left, right) => targetKeyOrder(left) - targetKeyOrder(right))
    .filter((target) => target.providerId && target.model.trim())
    .map((target): CreateTarget => ({
      provider_id: target.providerId,
      model: target.model.trim(),
      enabled: target.enabled,
      priority: Number(target.priority),
      first_token_timeout_ms: secondsToMilliseconds(target.firstTokenTimeoutSeconds),
      target_retry_budget: Number(target.targetRetryBudget),
      target_cooldown_ms: secondsToMilliseconds(target.targetCooldownSeconds),
      config_mode: target.configMode,
      thinking_level_map:
        target.persisted || hasSharedGroupPeerWithThinkingMap(targets, target)
          ? target.thinkingLevelMap
          : target.thinkingLevelMap.filter((row) => row.source === 'overridden'),
    }))

  return cleanTargets.length === targets.length && cleanTargets.length > 0
    ? { targets: cleanTargets }
    : { targets: cleanTargets, error: 'incomplete-target' }
}
