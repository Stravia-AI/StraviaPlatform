<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { beforeNavigate, goto } from '$app/navigation'
import { resolve } from '$app/paths'
import type { Path } from '$app/types'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'
import CheckIcon from '@lucide/svelte/icons/check'
import CirclePlusIcon from '@lucide/svelte/icons/circle-plus'
import EllipsisIcon from '@lucide/svelte/icons/ellipsis'
import GripVerticalIcon from '@lucide/svelte/icons/grip-vertical'
import ImageIcon from '@lucide/svelte/icons/image'
import MergeIcon from '@lucide/svelte/icons/merge'
import MessageSquareIcon from '@lucide/svelte/icons/message-square'
import PlusIcon from '@lucide/svelte/icons/plus'
import SearchIcon from '@lucide/svelte/icons/search'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { tick, untrack } from 'svelte'
import { toast } from 'svelte-sonner'
import { admin, currencyQueryOptions, invalidateRouteCatalogs } from '#lib/admin-client.js'
import { modelIdFromCatalogId } from '#lib/catalog-model-id.js'
import { isRouteBindingError, localizeBackendErrorMessage, unrepresentableThinkingTarget } from '#lib/backend-error.js'
import { startDragAutoscroll } from '#lib/drag-autoscroll.js'
import { beginPointerDrag } from '#lib/pointer-drag.js'
import { idNameOption, type SearchComboboxOption } from '#lib/search-combobox-option.js'
import { formatList } from '#lib/format.js'
import { currencyForKind } from '#lib/money.js'
import { routePurposeLabels } from '#lib/logical-model.js'
import { routePurposes, type RoutePurpose } from '#lib/types.js'
import PriceEditor from '#lib/components/price-editor.svelte'
import { buildModelCostJson, emptyPriceForm, priceFormFromCost, type PriceForm } from '#lib/components/price-form.js'

import {
  thinkingControlContext,
  unrepresentableThinkingLevels,
  type ThinkingControlContext,
} from '#lib/thinking-control.js'
import type {
  CurrencyKind,
  Provider,
  ProviderModelSummary,
  Route,
  RouteBilling,
  RouteSelectionStrategy,
  TargetRuntimeStatus,
  ThinkingLevel,
  ThinkingLevelMapping,
} from '#lib/types.js'
import {
  addRouteTarget,
  buildRouteTargets,
  countDistinctTargetAccounts,
  createRouteTarget,
  createRouteTargetForms,
  extractTargetGroupConfig,
  groupLaneTargetsByModel,
  inheritGroupConfigForNewTarget,
  mergeLaneIntoUpper,
  moveModelGroupToDock,
  moveModelGroupToInsertion,
  moveModelGroupToLane,
  moveRouteTargetToDock,
  moveRouteTargetToInsertion,
  moveTargetToLaneWithInheritance,
  priorityLanes,
  providersForRoutePurpose,
  removeRouteTarget,
  reorderRouteTargetBefore,
  routeSupportedThinkingLevels,
  stepModelGroup,
  stepTargetSplitMerge,
  summarizeTargetGroupConfig,
  swapAdjacentLanes,
  THINKING_LEVELS,
  updateModelGroupConfig,
  type RouteTargetForm,
  type RouteTargetGroupScope,
  type RouteTargetInsertion,
  type RouteTargetModelGroup,
  type TargetGroupConfig,
} from './route-targets-form.js'

import BatchAddSheet from './model-editor/batch-add-sheet.svelte'
import GroupConfigSheet from './model-editor/group-config-sheet.svelte'
import TargetDetailSheet from './model-editor/target-detail-sheet.svelte'
import ModelSpecification from '#lib/components/model-specification.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import SearchCombobox from '#lib/components/search-combobox.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import { SegmentedControl, type SegmentedControlOption } from '#lib/components/ui/segmented-control/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'

interface Props {
  model?: Route
  providers: Provider[]
  initialProviderId?: string
  initialModelId?: string
  onSaved?: () => void
}

type BillingMode = 'none' | 'per_call' | 'per_token'

type ActiveDrag =
  | { kind: 'target'; key: string }
  | { kind: 'group'; keys: string[]; model: string }
  | null

interface GroupSheetState {
  open: boolean
  sessionId: number
  scope: RouteTargetGroupScope
  model: string
}

interface BatchAddSheetState {
  open: boolean
  sessionId: number
  scope: RouteTargetGroupScope
  model: string
}

const UNSPECIFIED_THINKING_LEVEL = 'unspecified'
const purposeIcons = { inference: MessageSquareIcon, image_generation: ImageIcon, web_search: SearchIcon }

const DROP_ZONE_SELECTOR = [
  '[data-slot="target-priority-connector"]',
  '[data-slot="target-card"][data-enabled="true"]',
  '[data-slot="target-lane"]',
  '[data-slot="target-dock"]',
].join(', ')

let { model, providers, initialProviderId = '', initialModelId = '', onSaved }: Props = $props()
const initialModel = untrack(() => model)
const queryClient = useQueryClient()

let form = $state({
  modelId: initialModel?.model_id ?? '',
  purpose: initialModel ? initialModel.purpose : 'inference',
  displayName: initialModel?.display_name ?? '',
  balance: initialModel?.balance ?? 'traffic_equalization',
  enabled: initialModel?.is_enabled ?? true,
  defaultThinkingLevel: initialModel?.default_thinking_level ?? UNSPECIFIED_THINKING_LEVEL,
})

const initialBilling = untrack(() => initialModel?.billing ?? null)
let billingMode = $state<BillingMode>(initialBilling?.mode ?? 'none')
let billingAmount = $state(initialBilling?.mode === 'per_call' ? initialBilling.amount : '')
let billingCurrency = $state<CurrencyKind>(initialBilling?.mode === 'per_call' ? initialBilling.currency : 'usd')
let billingPrice = $state<PriceForm>(
  initialBilling?.mode === 'per_token' ? priceFormFromCost(initialBilling.price) : emptyPriceForm(),
)
let billingErrors = $state<string[]>([])
const billingCurrencyQuery = createQuery(() => currencyQueryOptions())
const customCurrency = $derived(billingCurrencyQuery.data ?? null)
const billingAmountIssue = $derived.by(() => {
  if (billingMode !== 'per_call') return ''
  const value = billingAmount.trim()
  return /^(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/.test(value) && Number(value) >= 0
    ? ''
    : m.billing_amount_invalid()
})

let targets = $state<RouteTargetForm[]>(
  untrack(() => createRouteTargetForms(initialModel, initialProviderId, initialModelId)),
)
let saving = $state(false)
let saveError = $state('')
let initialized = $state(false)

let targetEditorOpen = $state(false)
let targetEditorTarget = $state<RouteTargetForm>()
let targetEditorIsNew = $state(false)
let targetEditorInitiallyEnabled = $state(false)
let targetEditorClosing = false

let groupSheet = $state<GroupSheetState>({
  open: false,
  sessionId: 0,
  scope: { enabled: false },
  model: '',
})

let batchAddSheet = $state<BatchAddSheetState>({
  open: false,
  sessionId: 0,
  scope: { enabled: false },
  model: '',
})

let activeDrag = $state<ActiveDrag>(null)
let leaveConfirmOpen = $state(false)
let pendingNavigation = $state<() => void | Promise<void>>()

function draftTarget(target: RouteTargetForm) {
  return {
    key: target.key,
    id: target.id,
    providerId: target.providerId,
    model: target.model,
    enabled: target.enabled,
    priority: target.priority,
    configMode: target.configMode,
    firstTokenTimeoutSeconds: target.firstTokenTimeoutSeconds,
    targetRetryBudget: target.targetRetryBudget,
    targetCooldownSeconds: target.targetCooldownSeconds,
    thinkingLevelMap: target.thinkingLevelMap.map((row) => ({ ...row, control: { ...row.control } })),
  }
}

function draftSnapshot() {
  const editedTarget = targetEditorOpen ? targetEditorTarget : undefined
  return {
    form: { ...form },
    billing: {
      mode: billingMode,
      amount: billingAmount,
      currency: billingCurrency,
      price: $state.snapshot(billingPrice),
    },
    targets: targets
      .map((target) => draftTarget(editedTarget?.key === target.key ? editedTarget : target))
      .toSorted((left, right) => left.key.localeCompare(right.key)),
  }
}

let savedDraft = $state.raw(untrack(draftSnapshot))
const draftChanged = $derived(JSON.stringify(draftSnapshot()) !== JSON.stringify(savedDraft))

beforeNavigate((navigation) => {
  if (navigation.shallow && navigation.type === 'goto') return
  if (!draftChanged) return

  navigation.cancel()

  if (navigation.willUnload || !navigation.to?.url) return

  const { pathname, search, hash } = navigation.to.url
  const href = `${pathname.slice(resolve('').length)}${search}${hash}` as Path

  pendingNavigation = navigation.type === 'popstate' ? () => history.go(navigation.delta) : () => goto(resolve(href))

  leaveConfirmOpen = true
})

const availableProviders = $derived(providersForRoutePurpose(providers, form.purpose))
const targetLanes = $derived(priorityLanes(targets))
const enabledTargetCount = $derived(targets.filter((target) => target.enabled).length)
const disabledTargets = $derived(targets.filter((target) => !target.enabled))
const disabledGroups = $derived(groupLaneTargetsByModel(disabledTargets, { enabled: false }))
const isDragging = $derived(activeDrag !== null)
const draggedKeys = $derived.by(() => {
  if (!activeDrag) return new Set<string>()
  return activeDrag.kind === 'target' ? new Set([activeDrag.key]) : new Set(activeDrag.keys)
})

const knownInventories = $derived(
  new Map(
    targets
      .filter((target) => target.providerId && !target.loading && target.inventory.length > 0)
      .toReversed()
      .map((target) => [target.providerId, target.inventory] as const),
  ),
)

$effect(() => {
  if (isDragging) return startDragAutoscroll()
})

const canonicalModelsQuery = createQuery(() => ({
  queryKey: ['catalog', 'canonical-models'],
  queryFn: () => admin.catalog.canonicalModels(),
}))

const canonicalModels = $derived(canonicalModelsQuery.data?.models ?? [])

// Destination runtime status is a read-only projection of the saved route's circuit state;
// it refreshes on its own cadence and never writes back into the form draft.
const TARGET_STATUS_REFRESH_MS = 2_000
const savedRouteId = $derived(model?.model_id ?? '')
const targetStatusesQuery = createQuery(() => ({
  queryKey: ['model-target-statuses', savedRouteId],
  queryFn: () => admin.models.targetStatuses(savedRouteId),
  enabled: Boolean(savedRouteId),
  refetchInterval: (query) => (query.state.status === 'error' ? false : TARGET_STATUS_REFRESH_MS),
  retry: false,
}))
const targetStatusesFailed = $derived(Boolean(savedRouteId) && targetStatusesQuery.isError)
const targetStatusMap = $derived.by(() => {
  const statuses = Object.create(null) as Record<string, TargetRuntimeStatus>
  if (targetStatusesFailed) return statuses
  for (const status of targetStatusesQuery.data ?? []) {
    if (status.target_id) statuses[status.target_id] = status
  }
  return statuses
})

function targetRuntimeStatus(target: RouteTargetForm): TargetRuntimeStatus | undefined {
  if (!target.persisted || !target.id) return undefined
  const status = targetStatusMap[target.id]
  const modelId = target.model.trim()
  if (!status || status.provider_id !== target.providerId || status.model !== modelId) return undefined
  return status
}

function targetStateLabel(state: TargetRuntimeStatus['state'], cooldownMs: number | null): string {
  const label = {
    available: m.model_editor_target_status_available(),
    cooling_down: m.model_editor_target_status_cooling_down(),
    half_open: m.model_editor_target_status_half_open(),
    probing: m.model_editor_target_status_probing(),
  }[state]
  const seconds = cooldownMs === null ? 0 : Math.ceil(cooldownMs / 1000)
  return state === 'cooling_down' && seconds > 0
    ? m.model_editor_target_status_cooldown_remaining({ label, seconds })
    : label
}

const balanceOptions = $derived<readonly SegmentedControlOption<RouteSelectionStrategy>[]>([
  { value: 'traffic_equalization', label: m.model_editor_traffic_equalization() },
  { value: 'latency_preference', label: m.model_editor_latency_preference() },
])

const billingModeOptions = $derived<readonly SegmentedControlOption<BillingMode>[]>([
  { value: 'none', label: m.billing_mode_none() },
  { value: 'per_call', label: m.billing_mode_per_call() },
  { value: 'per_token', label: m.billing_mode_per_token() },
])

function strategyHelp(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'traffic_equalization':
      return m.model_editor_traffic_equalization_help()
    case 'latency_preference':
      return m.model_editor_latency_preference_help()
  }
}

$effect(() => {
  if (!initialized && providers.length > 0) {
    initialized = true
    for (const target of targets) {
      if (target.providerId) void loadInventory(target, true)
    }
  }
})

function modelCandidates(target: RouteTargetForm): ProviderModelSummary[] {
  const purpose = form.purpose
  return purpose ? target.inventory.filter((item) => item.route_purposes?.includes(purpose)) : []
}

function targetPurposeMismatch(target: RouteTargetForm): boolean {
  if (!form.purpose || !target.model) return false
  const savedTarget = initialModel?.targets.find(
    (item) =>
      target.persisted &&
      item.id === target.id &&
      item.provider_id === target.providerId &&
      item.model === target.model,
  )
  if (savedTarget) {
    return !savedTarget.route_purposes.includes(form.purpose)
  }
  if (target.loading) return false
  const summary = selectedSummary(target)
  if (summary) return !summary.route_purposes?.includes(form.purpose)
  return false
}

const purposeBlockers = $derived(targets.filter(targetPurposeMismatch))
const bindingBlocker = $derived.by(() => {
  if (!form.purpose || form.purpose === model?.purpose) return
  if (model?.image_generation_bound && form.purpose !== 'image_generation') return 'image_generation'
  if (model?.web_search_bound && form.purpose !== 'web_search') return 'web_search'
})

function selectedSummary(target: RouteTargetForm): ProviderModelSummary | undefined {
  return target.inventory.find((item) => item.id === target.model)
}

async function loadInventory(
  target: RouteTargetForm,
  initializeDraft = false,
  preserveThinkingMap = false,
): Promise<void> {
  if (!target.providerId) return
  target.loading = true
  target.validationError = ''
  try {
    target.inventory = (await admin.providers.models(target.providerId)).models
    if (target.model) {
      const summary = selectedSummary(target)
      target.custom = !summary
      if (summary?.name.trim()) target.modelName = summary.name.trim()
      if (summary && !target.persisted && (!preserveThinkingMap || target.thinkingLevelMap.length === 0)) {
        await loadThinkingMap(target)
        // 初次加载的映射不是表单编辑；只推进该字段基线，保留加载期间的其他草稿。
        if (initializeDraft) acceptImmediateThinkingMap(target, target.thinkingLevelMap)
      }
    }
  } catch (error) {
    target.inventory = []
    target.validationError = localizeBackendErrorMessage(error)
  } finally {
    target.loading = false
  }
}

async function changeProvider(target: RouteTargetForm, providerId: string): Promise<void> {
  if (target.providerId === providerId) return
  target.providerId = providerId
  target.model = ''
  target.modelName = null
  target.inventory = []
  target.custom = false
  target.persisted = false
  target.validationError = ''
  target.thinkingLevelMap = []
  await loadInventory(target)
}

async function selectModel(target: RouteTargetForm, modelId: string): Promise<void> {
  target.model = modelId
  target.custom = false
  target.validationError = ''
  target.thinkingLevelMap = []
  const summary = selectedSummary(target)
  target.modelName = summary?.name.trim() || null
  const scope: RouteTargetGroupScope = target.enabled ? { enabled: true, priority: target.priority } : { enabled: false }
  const peers = targets.filter((candidate) => candidate.key !== target.key)
  const inherited = target.configMode === 'shared' && inheritGroupConfigForNewTarget(peers, target, scope)
  if (!inherited || target.thinkingLevelMap.length === 0) {
    await loadThinkingMap(target)
  }
}

async function loadThinkingMap(target: RouteTargetForm): Promise<void> {
  if (!target.providerId || !target.model) return
  target.loading = true
  try {
    const detail = await admin.providers.model(target.providerId, target.model)
    target.thinkingLevelMap = detail.thinking_level_map?.map((row) => ({ ...row, control: { ...row.control } })) ?? []
  } catch (error) {
    target.validationError = m.model_editor_model_details_load_failed({ error: localizeBackendErrorMessage(error) })
  } finally {
    target.loading = false
  }
}

function useInventory(target: RouteTargetForm): void {
  target.custom = false
  target.model = ''
  target.modelName = null
  target.validationError = ''
}

function addTarget(): void {
  const target = addRouteTarget(targets)
  editTarget(target, true)
}

function cloneTarget(target: RouteTargetForm): RouteTargetForm {
  return {
    ...target,
    inventory: [...target.inventory],
    thinkingLevelMap: target.thinkingLevelMap.map((row) => ({ ...row, control: { ...row.control } })),
  }
}

function editTarget(target: RouteTargetForm, isNew = false): void {
  groupSheet.open = false
  batchAddSheet.open = false
  targetEditorClosing = false
  targetEditorTarget = cloneTarget(target)
  targetEditorIsNew = isNew
  targetEditorInitiallyEnabled = target.enabled
  targetEditorOpen = true
  if (targetEditorTarget.providerId && (target.loading || targetEditorTarget.inventory.length === 0)) {
    void loadInventory(targetEditorTarget)
  }
}

function closeTargetEditor(save: boolean): void {
  const target = targetEditorTarget
  if (save && target?.enabled) {
    const levels = unwritableThinkingLevels(target)
    if (levels.length > 0) {
      target.validationError = m.model_editor_thinking_enable_blocked({ levels: formatList(levels) })
      return
    }
  }
  targetEditorClosing = true
  if (target) {
    const index = targets.findIndex((candidate) => candidate.key === target.key)
    if (save && index >= 0) {
      const previous = targets[index]
      const scopeChanged =
        previous.enabled !== target.enabled ||
        previous.priority !== target.priority ||
        previous.model.trim() !== target.model.trim()
      if (scopeChanged && target.configMode === 'shared') {
        const nextScope: RouteTargetGroupScope = target.enabled
          ? { enabled: true, priority: target.priority }
          : { enabled: false }
        const peers = targets.filter((candidate) => candidate.key !== target.key)
        inheritGroupConfigForNewTarget(peers, target, nextScope)
      }
      Object.assign(targets[index], cloneTarget(target))
    } else if (!save && targetEditorIsNew && index >= 0) {
      removeRouteTarget(targets, index)
    }
  }
  targetEditorTarget = undefined
  targetEditorIsNew = false
  targetEditorInitiallyEnabled = false
  targetEditorOpen = false
}

function deleteEditedTarget(): void {
  const target = targetEditorTarget
  if (!target || target.enabled) return
  targetEditorClosing = true
  const index = targets.findIndex((candidate) => candidate.key === target.key)
  if (index >= 0) removeRouteTarget(targets, index)
  targetEditorTarget = undefined
  targetEditorIsNew = false
  targetEditorOpen = false
}

function removeDisabledTarget(target: RouteTargetForm): void {
  if (target.enabled) return
  const index = targets.findIndex((candidate) => candidate.key === target.key)
  if (index >= 0) removeRouteTarget(targets, index)
}

function acceptImmediateThinkingMap(target: RouteTargetForm, thinkingLevelMap: ThinkingLevelMapping[]): void {
  const nextMap = thinkingLevelMap.map((row) => ({ ...row, control: { ...row.control } }))
  target.thinkingLevelMap = nextMap
  const currentTarget = targets.find((candidate) => candidate.key === target.key)
  if (currentTarget) currentTarget.thinkingLevelMap = nextMap
  savedDraft = {
    ...savedDraft,
    targets: savedDraft.targets.map((candidate) =>
      candidate.key === target.key ? { ...candidate, thinkingLevelMap: nextMap } : candidate,
    ),
  }
}

async function discardDraftAndLeave(): Promise<void> {
  const navigate = pendingNavigation

  pendingNavigation = undefined
  leaveConfirmOpen = false

  if (!navigate) return

  savedDraft = draftSnapshot()
  await tick()
  await navigate()
}

function keepEditing(): void {
  pendingNavigation = undefined
}

function targetIndex(target: RouteTargetForm): number {
  return targets.findIndex((candidate) => candidate.key === target.key)
}

function targetProviderLabel(target: RouteTargetForm): string {
  return (
    providers.find((provider) => provider.id === target.providerId)?.name ||
    target.providerId ||
    m.model_editor_unconfigured_target()
  )
}

function targetModelIdentity(
  target: RouteTargetForm,
  summary: ProviderModelSummary | undefined,
): { label: string; technical: boolean } {
  const modelId = target.model.trim()
  const name = summary?.name.trim() || target.modelName?.trim()
  if (name) return { label: name, technical: name === modelId }
  return modelId ? { label: modelId, technical: true } : { label: m.model_editor_choose_model(), technical: false }
}

function groupModelSummary(group: RouteTargetModelGroup): ProviderModelSummary | undefined {
  for (const member of group.targets) {
    const summary = selectedSummary(member)
    if (summary) return summary
  }
  return undefined
}

function groupDisplayName(group: RouteTargetModelGroup, summary: ProviderModelSummary | undefined): string {
  const fromSummary = summary?.name.trim()
  if (fromSummary) return fromSummary
  const fromTarget = group.targets.find((member) => member.modelName?.trim())?.modelName?.trim()
  if (fromTarget) return fromTarget
  return group.model.trim() || m.model_editor_choose_model()
}

function formatThinkingRange(levels: readonly ThinkingLevel[]): string {
  if (levels.length === 0) return ''
  if (levels.length === 1) return levels[0]
  return `${levels[0]}–${levels[levels.length - 1]}`
}

function formatGroupConfigSummary(config: TargetGroupConfig): string {
  const summary = summarizeTargetGroupConfig(config)
  const parts = [
    `${summary.timeoutSeconds}${m.model_editor_unit_seconds()}`,
    m.model_editor_summary_retries({ count: summary.retryBudget }),
    `${summary.cooldownSeconds}${m.model_editor_unit_seconds()}`,
  ]
  const range = formatThinkingRange(summary.enabledThinkingLevels)
  if (range) parts.push(m.model_editor_thinking_summary_prefix({ range }))
  return parts.join(' · ')
}

function layerStatsLabel(groupCount: number, accountCount: number): string {
  if (groupCount > 1) return m.model_editor_layer_stats({ groups: groupCount, accounts: accountCount })
  return accountCount === 1
    ? m.model_editor_layer_stats_single_account()
    : m.model_editor_layer_stats_single_group({ accounts: accountCount })
}

function targetIssueLabels(target: RouteTargetForm, summary: ProviderModelSummary | undefined): string[] {
  const labels: string[] = []
  if (target.persisted && summary && !summary.available) labels.push(m.model_editor_model_no_longer_available())
  if (!target.loading && unwritableThinkingLevels(target, summary).length > 0) {
    labels.push(m.model_editor_thinking_map_unwritable())
  }
  return labels
}

function targetConfigured(target: RouteTargetForm): boolean {
  return Boolean(target.providerId && target.model.trim())
}

function matchesScope(target: RouteTargetForm, scope: RouteTargetGroupScope): boolean {
  if (!scope.enabled) return !target.enabled
  return target.enabled && target.priority === scope.priority
}

function layerNumberForScope(scope: RouteTargetGroupScope): number | null {
  if (!scope.enabled) return null
  const idx = targetLanes.findIndex((lane) => lane.priority === scope.priority)
  return idx >= 0 ? idx + 1 : null
}

// Derived state for TargetDetailSheet's group context
const targetEditorGroupPeers = $derived.by(() => {
  const edited = targetEditorTarget
  if (!edited || !edited.model.trim()) return { count: 0, sharedPeerConfig: undefined as TargetGroupConfig | undefined }
  const normalized = edited.model.trim()
  const scope: RouteTargetGroupScope = edited.enabled
    ? { enabled: true, priority: edited.priority }
    : { enabled: false }
  const peers = targets.filter(
    (candidate) => candidate.key !== edited.key && matchesScope(candidate, scope) && candidate.model.trim() === normalized,
  )
  const sharedPeer = peers.find((candidate) => candidate.configMode === 'shared')
  return {
    count: peers.length + 1,
    sharedPeerConfig: sharedPeer ? extractTargetGroupConfig(sharedPeer) : undefined,
  }
})

// Derived state for GroupConfigSheet
const activeGroupMembers = $derived.by(() => {
  if (!groupSheet.open) return [] as RouteTargetForm[]
  const normalized = groupSheet.model.trim()
  return targets.filter(
    (candidate) => matchesScope(candidate, groupSheet.scope) && candidate.model.trim() === normalized,
  )
})

const activeGroupData = $derived.by(() => {
  if (activeGroupMembers.length === 0) return undefined
  const groups = groupLaneTargetsByModel(activeGroupMembers, groupSheet.scope)
  return groups[0]
})

const activeGroupSummary = $derived(activeGroupData ? groupModelSummary(activeGroupData) : undefined)
const activeGroupModelName = $derived(
  activeGroupData ? groupDisplayName(activeGroupData, activeGroupSummary) : groupSheet.model,
)
const activeGroupLayerNumber = $derived(layerNumberForScope(groupSheet.scope))

// Derived state for BatchAddSheet
const batchAddExistingProviders = $derived.by(() => {
  if (!batchAddSheet.open) return new Set<string>()
  const normalized = batchAddSheet.model.trim()
  return new Set(
    targets
      .filter((candidate) => matchesScope(candidate, batchAddSheet.scope) && candidate.model.trim() === normalized)
      .map((candidate) => candidate.providerId),
  )
})

const batchAddModelName = $derived.by(() => {
  const normalized = batchAddSheet.model.trim()
  const sample = targets.find((candidate) => candidate.model.trim() === normalized)
  if (!sample) return normalized
  const summary = selectedSummary(sample)
  return summary?.name.trim() || sample.modelName?.trim() || normalized
})
const batchAddLayerNumber = $derived(layerNumberForScope(batchAddSheet.scope))

function openGroupConfig(scope: RouteTargetGroupScope, modelId: string): void {
  targetEditorOpen = false
  targetEditorTarget = undefined
  batchAddSheet.open = false
  groupSheet = {
    open: true,
    sessionId: groupSheet.sessionId + 1,
    scope,
    model: modelId,
  }
}

function openBatchAdd(scope: RouteTargetGroupScope, modelId: string): void {
  if (!modelId.trim()) return
  targetEditorOpen = false
  targetEditorTarget = undefined
  groupSheet.open = false
  batchAddSheet = {
    open: true,
    sessionId: batchAddSheet.sessionId + 1,
    scope,
    model: modelId,
  }
}

function handleApplyGroupConfig(nextConfig: TargetGroupConfig, overwriteCustom: boolean): void {
  const result = updateModelGroupConfig(targets, groupSheet.scope, groupSheet.model, nextConfig, {
    overwriteCustom,
  })
  groupSheet.open = false
  if (result.updatedCount > 0) {
    toast.success(m.model_editor_group_updated_toast({ count: result.updatedCount }))
  }
}

function resolveGroupServerPriority(scope: RouteTargetGroupScope, members: RouteTargetForm[]): number | null {
  if (scope.enabled) return scope.priority
  const persisted = members.filter((member) => member.persisted && Boolean(member.id))
  if (persisted.length === 0) return null
  return persisted[0].priority
}

function applyServerGroupThinkingResponse(
  updated: Route,
  affectedIds: ReadonlySet<string>,
): ThinkingLevelMapping[] | null {
  let firstMap: ThinkingLevelMapping[] | null = null
  const updatedById = new Map(updated.targets.map((item) => [item.id, item]))
  const nextSavedTargets = savedDraft.targets.map((saved) => {
    if (!saved.id || !affectedIds.has(saved.id)) return saved
    const fresh = updatedById.get(saved.id)
    if (!fresh) return saved
    const cloned = fresh.thinking_level_map.map((row) => ({ ...row, control: { ...row.control } }))
    return { ...saved, thinkingLevelMap: cloned }
  })
  for (const target of targets) {
    if (!target.id || !affectedIds.has(target.id)) continue
    const fresh = updatedById.get(target.id)
    if (!fresh) continue
    const cloned = fresh.thinking_level_map.map((row) => ({ ...row, control: { ...row.control } }))
    target.thinkingLevelMap = cloned
    if (!firstMap) firstMap = cloned
  }
  savedDraft = { ...savedDraft, targets: nextSavedTargets }
  return firstMap
}

async function handleRegenerateGroupThinking(overwriteCustom: boolean): Promise<ThinkingLevelMapping[] | null> {
  if (!savedRouteId) return null
  const affected = overwriteCustom
    ? activeGroupMembers
    : activeGroupMembers.filter((member) => member.configMode === 'shared')
  const priority = resolveGroupServerPriority(groupSheet.scope, affected)
  if (priority === null) return null
  try {
    const updated = await admin.models.regenerateGroupThinkingMap(savedRouteId, {
      enabled: groupSheet.scope.enabled,
      priority,
      model: groupSheet.model,
      include_custom: overwriteCustom,
    })
    const affectedIds = new Set(affected.map((member) => member.id).filter((id): id is string => Boolean(id)))
    return applyServerGroupThinkingResponse(updated, affectedIds)
  } catch (error) {
    throw new Error(localizeBackendErrorMessage(error))
  }
}

async function handleResetGroupThinkingRow(
  level: ThinkingLevel,
  overwriteCustom: boolean,
): Promise<ThinkingLevelMapping[] | null> {
  if (!savedRouteId) return null
  const affected = overwriteCustom
    ? activeGroupMembers
    : activeGroupMembers.filter((member) => member.configMode === 'shared')
  const priority = resolveGroupServerPriority(groupSheet.scope, affected)
  if (priority === null) return null
  try {
    const updated = await admin.models.resetGroupThinkingMapping(savedRouteId, {
      enabled: groupSheet.scope.enabled,
      priority,
      model: groupSheet.model,
      level,
      include_custom: overwriteCustom,
    })
    const affectedIds = new Set(affected.map((member) => member.id).filter((id): id is string => Boolean(id)))
    return applyServerGroupThinkingResponse(updated, affectedIds)
  } catch (error) {
    throw new Error(localizeBackendErrorMessage(error))
  }
}

function handleBatchAddSubmit(
  providerIds: string[],
  loadedInventories: ReadonlyMap<string, ProviderModelSummary[]>,
): void {
  const scope = batchAddSheet.scope
  const modelId = batchAddSheet.model.trim()
  const modelName = batchAddModelName !== modelId ? batchAddModelName : null
  batchAddSheet.open = false
  if (providerIds.length === 0 || !modelId) return

  const created: { target: RouteTargetForm; inherited: boolean }[] = []
  for (const providerId of providerIds) {
    const preloaded = loadedInventories.get(providerId)
    const draft = createRouteTarget(targets, {
      providerId,
      model: modelId,
      modelName,
      enabled: scope.enabled,
      priority: scope.enabled ? scope.priority : 0,
      configMode: 'shared',
      inventory: preloaded ? [...preloaded] : [],
    })
    const inherited = inheritGroupConfigForNewTarget(targets, draft, scope)
    targets.push(draft)
    created.push({ target: draft, inherited })
  }

  void Promise.all(
    created.map(async ({ target, inherited }) => {
      if (target.inventory.length > 0) {
        const summary = selectedSummary(target)
        target.custom = !summary
        const trimmedName = summary?.name?.trim()
        if (trimmedName) target.modelName = trimmedName
        if (summary && (!inherited || target.thinkingLevelMap.length === 0)) {
          await loadThinkingMap(target)
        }
      } else {
        await loadInventory(target, false, inherited)
      }
    }),
  )

  toast.success(m.model_editor_batch_add_added_toast({ count: created.length }))
}

// ==================== Unified Pointer Drag Engine ====================

async function dropSingleTarget(key: string, zone: Element): Promise<void> {
  const slot = zone.getAttribute('data-slot')
  if (!slot) return
  if (slot === 'target-dock') {
    if (!moveRouteTargetToDock(targets, key)) {
      toast.error(m.model_editor_last_enabled_target_required())
    }
    return
  }

  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return

  if (slot === 'target-priority-connector') {
    if (!targetConfigured(target)) {
      toast.error(m.model_editor_complete_target_before_enabling())
      return
    }
    if (!target.enabled && (await blockEnableForUnwritableThinking(target))) return
    const insertion = insertionFromZone(zone)
    if (!insertion) return
    if (!moveRouteTargetToInsertion(targets, key, insertion)) {
      toast.error(m.model_editor_cannot_create_priority_layer())
    }
    return
  }

  const priority = Number(zone.closest('[data-slot="target-lane"]')?.getAttribute('data-priority'))
  if (!Number.isFinite(priority)) return
  const beforeKey = slot === 'target-card' ? (zone.getAttribute('data-key') ?? undefined) : undefined

  if (beforeKey && target.enabled && target.priority === priority) {
    reorderRouteTargetBefore(targets, key, beforeKey)
    return
  }
  if (!target.enabled && (await blockEnableForUnwritableThinking(target))) return
  const moved = moveTargetToLaneWithInheritance(targets, key, priority)
  if (!moved.ok) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  if (moved.inheritedFromGroup) {
    toast.info(m.model_editor_group_inherited_toast({ provider: targetProviderLabel(target) }))
  }
}

async function dropModelGroup(keys: readonly string[], zone: Element): Promise<void> {
  const slot = zone.getAttribute('data-slot')
  if (!slot) return
  if (slot === 'target-dock') {
    if (!moveModelGroupToDock(targets, keys)) {
      toast.error(m.model_editor_last_enabled_target_required())
    }
    return
  }

  const members = targets.filter((candidate) => keys.includes(candidate.key))
  if (members.length === 0) return
  if (members.some((member) => !targetConfigured(member))) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  for (const member of members) {
    if (!member.enabled && (await blockEnableForUnwritableThinking(member))) return
  }

  if (slot === 'target-priority-connector') {
    const insertion = insertionFromZone(zone)
    if (!insertion) return
    if (!moveModelGroupToInsertion(targets, keys, insertion)) {
      toast.error(m.model_editor_cannot_create_priority_layer())
    }
    return
  }

  const priority = Number(zone.closest('[data-slot="target-lane"]')?.getAttribute('data-priority'))
  if (!Number.isFinite(priority)) return
  const moved = moveModelGroupToLane(targets, keys, priority)
  if (!moved.ok) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  if (moved.syncedShared > 0) {
    toast.info(m.model_editor_group_synced_toast({ count: moved.syncedShared }))
  }
}

function insertionFromZone(zone: Element): RouteTargetInsertion | null {
  const position = zone.getAttribute('data-position') ?? ''
  if (position === 'top' || position === 'empty') return { position: 'top' }
  if (position === 'bottom') return { position: 'bottom' }
  const upperPriority = Number(zone.getAttribute('data-upper-priority'))
  const lowerPriority = Number(zone.getAttribute('data-lower-priority'))
  if (!Number.isFinite(upperPriority) || !Number.isFinite(lowerPriority)) return null
  return { position: 'between', upperPriority, lowerPriority }
}

function startTargetPointerDrag(event: PointerEvent, target: RouteTargetForm, fromHandle: boolean): void {
  if (!fromHandle) {
    if (event.pointerType === 'touch') return
    if ((event.target as Element | null)?.closest('button, a, input, select')) return
  } else {
    event.stopPropagation()
  }
  const source = (event.currentTarget as HTMLElement).closest<HTMLElement>('[data-slot="target-card"]')
  if (!source) return
  beginPointerDrag(event, {
    source,
    zoneSelector: DROP_ZONE_SELECTOR,
    onStart: () => {
      activeDrag = { kind: 'target', key: target.key }
    },
    onDrop: async (zone) => {
      try {
        if (zone) await dropSingleTarget(target.key, zone)
      } finally {
        activeDrag = null
      }
    },
    onCancel: () => {
      activeDrag = null
    },
  })
}

function startGroupPointerDrag(event: PointerEvent, group: RouteTargetModelGroup): void {
  event.stopPropagation()
  const source = (event.currentTarget as HTMLElement).closest<HTMLElement>('[data-slot="target-model-group"]')
  if (!source) return
  const keys = group.targets.map((member) => member.key)
  beginPointerDrag(event, {
    source,
    zoneSelector: DROP_ZONE_SELECTOR,
    onStart: () => {
      activeDrag = { kind: 'group', keys, model: group.model }
    },
    onDrop: async (zone) => {
      try {
        if (zone) await dropModelGroup(keys, zone)
      } finally {
        activeDrag = null
      }
    },
    onCancel: () => {
      activeDrag = null
    },
  })
}

function handleTargetCardKeydown(event: KeyboardEvent, target: RouteTargetForm): void {
  if (event.target !== event.currentTarget) return
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault()
    editTarget(target)
    return
  }
  if (event.altKey && (event.key === 'ArrowUp' || event.key === 'ArrowDown') && target.enabled) {
    event.preventDefault()
    const result = stepTargetSplitMerge(targets, target.key, event.key === 'ArrowUp' ? 'up' : 'down')
    if (result.ok && result.inheritedFromGroup) {
      toast.info(m.model_editor_group_inherited_toast({ provider: targetProviderLabel(target) }))
    }
  }
}

// ==================== Layer & Group Quick Actions ====================

function handleSwapLanes(upperPriority: number, lowerPriority: number): void {
  swapAdjacentLanes(targets, upperPriority, lowerPriority)
}

function handleMergeLaneIntoUpper(lowerPriority: number, upperPriority: number): void {
  const result = mergeLaneIntoUpper(targets, lowerPriority, upperPriority)
  if (result.syncedShared > 0) {
    toast.info(m.model_editor_group_synced_toast({ count: result.syncedShared }))
  }
}

function handleStepGroup(group: RouteTargetModelGroup, direction: 'up' | 'down'): void {
  stepModelGroup(
    targets,
    group.targets.map((member) => member.key),
    direction,
  )
}

function handleMoveGroupToExistingLayer(group: RouteTargetModelGroup, targetPriority: number): void {
  const moved = moveModelGroupToLane(
    targets,
    group.targets.map((member) => member.key),
    targetPriority,
  )
  if (!moved.ok) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  if (moved.syncedShared > 0) {
    toast.info(m.model_editor_group_synced_toast({ count: moved.syncedShared }))
  }
}

function handleSplitGroupToNewLayer(group: RouteTargetModelGroup, laneIndex: number): void {
  const keys = group.targets.map((member) => member.key)
  const currentLane = targetLanes[laneIndex]
  if (!currentLane) return
  const insertion: RouteTargetInsertion =
    laneIndex < targetLanes.length - 1
      ? { position: 'between', upperPriority: currentLane.priority, lowerPriority: targetLanes[laneIndex + 1].priority }
      : { position: 'bottom' }
  if (!moveModelGroupToInsertion(targets, keys, insertion)) {
    toast.error(m.model_editor_cannot_create_priority_layer())
  }
}

function handleDisableGroup(group: RouteTargetModelGroup): void {
  if (
    !moveModelGroupToDock(
      targets,
      group.targets.map((member) => member.key),
    )
  ) {
    toast.error(m.model_editor_last_enabled_target_required())
  }
}

function insertionLabel(
  insertion: RouteTargetInsertion,
  upperLayer: number | null,
  lowerLayer: number | null,
): string {
  if (insertion.position === 'top') return m.model_editor_insert_higher_priority()
  if (insertion.position === 'bottom') return m.model_editor_insert_lower_priority()
  if (upperLayer !== null && lowerLayer !== null) {
    return m.model_editor_insert_between_layers({ upper: upperLayer, lower: lowerLayer })
  }
  return m.model_editor_insert_new_priority()
}

// ==================== Thinking Levels & Validation ====================

function targetSupportsThinkingLevel(target: RouteTargetForm, level: ThinkingLevel): boolean {
  return target.thinkingLevelMap.some((row) => row.level === level && row.control.type !== 'hidden')
}

const supportedThinkingLevels = $derived(routeSupportedThinkingLevels(targets))
const defaultThinkingLevelUnsupported = $derived(
  form.defaultThinkingLevel !== UNSPECIFIED_THINKING_LEVEL &&
    !supportedThinkingLevels.includes(form.defaultThinkingLevel as ThinkingLevel),
)

function defaultThinkingLevelLabel(): string {
  return form.defaultThinkingLevel === UNSPECIFIED_THINKING_LEVEL
    ? m.model_editor_thinking_level_provider_default()
    : form.defaultThinkingLevel
}

function targetLabel(target: RouteTargetForm, index: number): string {
  const destination = m.model_editor_destination_value({ index: index + 1 })
  const provider = providers.find((candidate) => candidate.id === target.providerId)
  const modelId = target.model.trim()
  return [destination, provider?.name ?? target.providerId, modelId].filter(Boolean).join(' · ')
}

function thinkingLevelBlockers(level: ThinkingLevel): string[] {
  return targets.flatMap((target, index) =>
    !target.enabled ? [] : targetSupportsThinkingLevel(target, level) ? [] : [targetLabel(target, index)],
  )
}

function targetThinkingContext(
  target: RouteTargetForm,
  summary: ProviderModelSummary | undefined = selectedSummary(target),
): ThinkingControlContext {
  return thinkingControlContext(
    providers.find((provider) => provider.id === target.providerId),
    summary?.specification?.reasoning_efforts,
  )
}

function unwritableThinkingLevels(target: RouteTargetForm, summary = selectedSummary(target)): ThinkingLevel[] {
  return unrepresentableThinkingLevels(target.thinkingLevelMap, targetThinkingContext(target, summary))
}

async function blockEnableForUnwritableThinking(target: RouteTargetForm): Promise<boolean> {
  if (target.thinkingLevelMap.length === 0 && target.providerId && target.model.trim()) {
    await loadThinkingMap(target)
  }
  const levels = unwritableThinkingLevels(target)
  if (levels.length === 0) return false
  toast.error(m.model_editor_thinking_enable_blocked({ levels: formatList(levels) }))
  activeDrag = null
  editTarget(target)
  return true
}

async function resetThinkingRow(target: RouteTargetForm, level: ThinkingLevel): Promise<void> {
  if (!initialModel || !target.id) return
  target.validationError = ''
  try {
    const updated = await admin.models.resetThinkingMapping(initialModel.model_id, target.id, level)
    acceptImmediateThinkingMap(
      target,
      updated.targets.find((candidate) => candidate.id === target.id)?.thinking_level_map ?? target.thinkingLevelMap,
    )
  } catch (error) {
    target.validationError = localizeBackendErrorMessage(error)
  }
}

async function regenerateThinkingMap(target: RouteTargetForm): Promise<void> {
  if (!initialModel || !target.id) return

  target.validationError = ''
  try {
    const updated = await admin.models.regenerateThinkingMap(initialModel.model_id, target.id)
    acceptImmediateThinkingMap(
      target,
      updated.targets.find((candidate) => candidate.id === target.id)?.thinking_level_map ?? target.thinkingLevelMap,
    )
  } catch (error) {
    target.validationError = localizeBackendErrorMessage(error)
  }
}

// ==================== Save Model ====================

async function saveModel(): Promise<void> {
  saveError = ''
  billingErrors = []
  let billing: RouteBilling | null = null
  if (billingMode === 'per_call') {
    if (billingAmountIssue) {
      saveError = billingAmountIssue
      return
    }
    billing = { mode: 'per_call', amount: billingAmount.trim(), currency: billingCurrency }
  } else if (billingMode === 'per_token') {
    const result = buildModelCostJson($state.snapshot(billingPrice))
    if (!result.value) {
      billingErrors = result.errors
      saveError = result.errors[0] ?? m.billing_price_invalid()
      return
    }
    billing = { mode: 'per_token', price: result.value }
  }
  if (!form.purpose) {
    saveError = m.route_purpose_reconfigure()
    return
  }
  if (bindingBlocker) {
    saveError =
      bindingBlocker === 'image_generation'
        ? m.backend_error_route_image_generation_bound()
        : m.backend_error_route_web_search_bound()
    return
  }
  const purposeChanged = initialModel?.purpose !== form.purpose
  if (purposeChanged && purposeBlockers.length > 0) {
    saveError = `${m.route_purpose_blockers()} ${formatList(
      purposeBlockers.map((target) => `${targetProviderLabel(target)} · ${target.model}`),
    )}`
    toast.error(saveError)
    return
  }
  const targetsChanged = !initialModel || JSON.stringify(draftSnapshot().targets) !== JSON.stringify(savedDraft.targets)
  const result = targetsChanged ? buildRouteTargets(targets) : undefined
  if (result?.error && result.error !== 'incomplete-target' && result.error !== 'no-enabled-target') {
    toast.error(m.model_editor_invalid_target_controls())
    return
  }
  const cleanTargets = result?.targets
  if (!form.modelId.trim() || (targetsChanged && (!cleanTargets?.some((target) => target.enabled) || result?.error))) {
    toast.error(m.model_editor_enabled_destination_required())
    return
  }

  const blocked = targetsChanged
    ? targets.find((target) => target.enabled && unwritableThinkingLevels(target).length > 0)
    : undefined
  if (blocked) {
    toast.error(m.model_editor_thinking_enable_blocked({ levels: formatList(unwritableThinkingLevels(blocked)) }))
    editTarget(blocked)
    return
  }

  saving = true
  try {
    if (targetsChanged) {
      for (const target of targets) {
        const modelId = target.model.trim()
        const needsSnapshot =
          target.custom && !target.persisted && !target.inventory.some((providerModel) => providerModel.id === modelId)
        if (needsSnapshot) {
          await admin.providers.createManualModel(
            target.providerId,
            modelId,
            JSON.stringify({ id: modelId, name: modelId }),
          )
        }
      }
    }

    const input = {
      model_id: form.modelId.trim(),
      purpose: form.purpose,
      display_name: form.displayName.trim() || null,
      balance: form.balance,
      default_thinking_level:
        form.defaultThinkingLevel === UNSPECIFIED_THINKING_LEVEL ? null : (form.defaultThinkingLevel as ThinkingLevel),
      billing,
    }

    if (initialModel) {
      await admin.models.update(initialModel.model_id, {
        ...input,
        ...(purposeChanged ? {} : { purpose: undefined }),
        is_enabled: form.enabled,
        ...(targetsChanged ? { targets: cleanTargets! } : {}),
      })
    } else {
      const created = await admin.models.create({ ...input, targets: cleanTargets! })
      if (!form.enabled) {
        try {
          await admin.models.update(created.model_id, { is_enabled: false })
        } catch {
          await invalidateRouteCatalogs(queryClient)
          toast.error(m.model_editor_model_was_added_but_not_disabled_review_status())
          savedDraft = draftSnapshot()
          await tick()
          await goto(resolve('/models/[id]', { id: created.model_id }))
          return
        }
      }
    }
    await Promise.all([
      invalidateRouteCatalogs(queryClient),
      queryClient.invalidateQueries({ queryKey: ['api-keys'] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
    ])
    savedDraft = draftSnapshot()
    await tick()
    toast.success(m.model_editor_model_saved())
    onSaved?.()
    if (!onSaved) await goto(resolve('models'))
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
    if (initialModel && isRouteBindingError(error)) {
      void queryClient.invalidateQueries({ queryKey: ['models', initialModel.model_id] })
    }
    const failed = unrepresentableThinkingTarget(error)
    const target = failed
      ? targets.find((candidate) => candidate.providerId === failed.providerId && candidate.model === failed.modelId)
      : undefined
    if (target) {
      target.validationError = localizeBackendErrorMessage(error)
      editTarget(target)
    }
  } finally {
    saving = false
  }
}
</script>

{#snippet priorityConnector(
  insertion: RouteTargetInsertion,
  position: string,
  upperLayer: number | null,
  lowerLayer: number | null,
)}
  {@const isBetween = insertion.position === 'between'}
  {@const label = insertionLabel(insertion, upperLayer, lowerLayer)}
  <div
    data-slot="target-priority-connector"
    data-position={position}
    data-upper-priority={insertion.position === 'between' ? insertion.upperPriority : undefined}
    data-lower-priority={insertion.position === 'between' ? insertion.lowerPriority : undefined}
    class={[
      'relative mx-auto flex w-full items-center justify-center rounded-lg border transition-[border-color,background-color,color] duration-150 motion-reduce:transition-none',
      'h-6',
      isDragging
        ? 'border-dashed border-primary/60 bg-primary/5 text-primary data-[drag-over=true]:border-primary data-[drag-over=true]:bg-primary/20'
        : 'border-transparent text-muted-foreground',
    ]}
    role="group"
    aria-label={label}>
    {#if isDragging}
      <span class="inline-flex items-center gap-1.5 rounded-md bg-background/90 px-2.5 py-0.5 text-xs font-medium">
        <PlusIcon class="size-3.5" aria-hidden="true" />
        {label}
      </span>
    {:else if isBetween}
      <span class="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
        <ArrowDownIcon class="size-3.5" aria-hidden="true" />
        {m.model_editor_layer_fallback_connector()}
      </span>
    {/if}
  </div>
{/snippet}

{#snippet disabledTargetCard(target: RouteTargetForm)}
  {@const summary = selectedSummary(target)}
  {@const issueLabels = targetIssueLabels(target, summary)}
  {@const modelIdentity = targetModelIdentity(target, summary)}
  {@const mismatch = targetPurposeMismatch(target)}
  {@const isCustom = target.configMode === 'custom'}
  {@const hasError = mismatch || issueLabels.length > 0}
  <div class="relative min-w-0">
    <div
      role="button"
      tabindex="0"
      aria-label={m.model_editor_edit_destination_value({ index: targetIndex(target) + 1 })}
      data-slot="target-card"
      data-enabled="false"
      data-key={target.key}
      data-config-mode={target.configMode}
      class={[
        'group flex w-full min-w-0 cursor-grab select-none flex-col items-start rounded-md border bg-background px-2.5 py-2 pr-10 text-left transition-[border-color,background-color,opacity] hover:border-primary/40 focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 active:cursor-grabbing motion-reduce:transition-none',
        hasError
          ? 'border-destructive/50 bg-destructive/[0.04]'
          : isCustom
            ? 'border-warning/50 bg-warning/[0.04]'
            : 'border-border',
        draggedKeys.has(target.key) && 'opacity-50',
      ]}
      onpointerdown={(event) => startTargetPointerDrag(event, target, false)}
      onclick={() => editTarget(target)}
      onkeydown={(event) => handleTargetCardKeydown(event, target)}>
      <div class="flex w-full min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <span
          data-slot="target-drag-handle"
          role="presentation"
          class="-my-1 -ml-1 flex size-7 shrink-0 touch-none items-center justify-center text-muted-foreground"
          onpointerdown={(event) => startTargetPointerDrag(event, target, true)}>
          <GripVerticalIcon class="size-3.5" aria-hidden="true" />
        </span>
        <span class={['min-w-0 font-medium wrap-anywhere', modelIdentity.technical && 'font-technical text-sm']}>
          {modelIdentity.label}
        </span>
        <span class="text-muted-foreground" aria-hidden="true">·</span>
        <span class="min-w-0 truncate text-sm text-muted-foreground">
          {targetProviderLabel(target)}
        </span>
        {#if isCustom}
          <span class="inline-flex shrink-0 items-center rounded border border-warning/40 bg-warning/10 px-1.5 py-0.5 text-[11px] font-medium leading-none text-warning">
            {m.model_editor_custom_badge()}
          </span>
        {/if}
        {#if mismatch}
          <StatusIndicator compact label={m.route_purpose_mismatch()} tone="error" />
        {/if}
      </div>
      {#if issueLabels.length > 0}
        <div class="mt-1.5 flex w-full min-w-0 flex-wrap gap-1.5 pl-6">
          {#each issueLabels as label (label)}
            <Badge variant="destructive">{label}</Badge>
          {/each}
        </div>
      {/if}
    </div>
    <Button
      type="button"
      variant="ghost"
      size="icon"
      class="absolute right-1 top-1/2 size-8 -translate-y-1/2"
      aria-label={m.model_editor_remove_destination_value({ index: targetIndex(target) + 1 })}
      onclick={() => removeDisabledTarget(target)}>
      <Trash2Icon />
    </Button>
  </div>
{/snippet}

<div class="route-page route-page-column min-h-[calc(100svh-5rem)]">
  <PageHeader title={initialModel ? initialModel.display_name?.trim() || initialModel.model_id : m.common_add_model()}>
    {#snippet meta()}
      {#if initialModel}
        <StatusIndicator
          compact
          label={initialModel.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
          tone={initialModel.is_enabled ? 'healthy' : 'neutral'} />
      {/if}
    {/snippet}
  </PageHeader>

  <form
    class="flex flex-1 flex-col gap-8"
    onsubmit={(event) => {
      event.preventDefault()
      void saveModel()
    }}>
    <section class="route-section @container/model-contract" aria-labelledby="route-contract-title">
      <div class="route-section-header">
        <div class="flex min-w-0 items-center gap-2">
          <h2 id="route-contract-title" class="route-section-title">{m.model_editor_client_model()}</h2>
          <Field.Help hint={m.model_editor_set_model_name_request_type_client_apps_use()} />
        </div>
      </div>
      <!-- 设置列够宽（≥42rem）时两两并排：Model ID｜显示名称、用途｜请求发送方式；开关行独占一行。 -->
      <Field.Group class="@2xl/model-contract:grid @2xl/model-contract:grid-cols-2 @2xl/model-contract:gap-x-6">
        <SettingRow title={m.common_enable_action()} layout="inline" class="py-0 @2xl/model-contract:col-span-2">
          {#snippet children({ titleId })}
            <div data-slot="model-enabled-control" class="flex shrink-0 items-center">
              <Switch
                id="route-enabled"
                aria-labelledby={titleId}
                aria-label={m.common_enable_action()}
                bind:checked={() => form.enabled, (checked: boolean) => (form.enabled = checked)} />
            </div>
          {/snippet}
        </SettingRow>
        <Field.Field>
          <Field.Label for="route-model-id">{m.model_editor_model_id()}</Field.Label>
          <SearchCombobox
            id="route-model-id"
            value={form.modelId}
            options={canonicalModels.map(idNameOption)}
            freeText
            placeholder={m.model_editor_model_id_placeholder()}
            emptyText={m.common_no_matching_models()}
            ariaLabel={m.model_editor_model_id()}
            clearLabel={m.model_editor_clear_selected_model()}
            onValueChange={(value: string, option?: SearchComboboxOption) => {
              form.modelId = option ? modelIdFromCatalogId(option.value) : value
              if (option) form.displayName = option.label
            }}
            onClear={() => (form.displayName = '')} />
        </Field.Field>
        <Field.Field>
          <Field.Label for="route-display-name">{m.model_editor_model_name()}</Field.Label>
          <Input
            id="route-display-name"
            bind:value={form.displayName}
            placeholder={m.model_editor_model_name_placeholder()} />
        </Field.Field>
        <Field.Field size="setting">
          <Field.Label for="route-purpose">{m.route_purpose_label()}</Field.Label>
          <Select.Root
            type="single"
            bind:value={
              () => form.purpose ?? undefined, (value: RoutePurpose | undefined) => (form.purpose = value ?? null)
            }>
            <Select.Trigger id="route-purpose" class="w-full" aria-label={m.route_purpose_label()}>
              {#if form.purpose}
                {@const Icon = purposeIcons[form.purpose]}
                <span class="flex min-w-0 items-center gap-2">
                  <Icon class="text-muted-foreground" aria-hidden="true" />
                  {routePurposeLabels[form.purpose]()}
                </span>
              {:else}
                {m.route_purpose_placeholder()}
              {/if}
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
                {#each routePurposes as value (value)}
                  {@const Icon = purposeIcons[value]}
                  {@const label = routePurposeLabels[value]()}
                  <Select.Item {value} {label} class="pr-1.5 [&_.cn-select-item-indicator-icon]:hidden">
                    <Icon class="text-muted-foreground" aria-hidden="true" />{label}
                  </Select.Item>
                {/each}
              </Select.Group>
            </Select.Content>
          </Select.Root>
        </Field.Field>
        <Field.Field>
          <Field.Label id="route-balance-label" hint={strategyHelp(form.balance)}>
            {m.model_editor_how_requests_sent()}
          </Field.Label>
          <SegmentedControl
            id="route-balance"
            options={balanceOptions}
            value={form.balance}
            onValueChange={(value: RouteSelectionStrategy) => {
              form.balance = value
            }}
            ariaLabelledby="route-balance-label"
            disabled={saving} />
        </Field.Field>
      </Field.Group>
      {#if initialModel && !initialModel.purpose}
        <p class="mt-3 text-sm text-warning" role="status">{m.route_purpose_reconfigure()}</p>
      {/if}
      {#if bindingBlocker}
        <p class="mt-3 text-sm text-warning" role="status">
          {#if bindingBlocker === 'image_generation'}
            {m.backend_error_route_image_generation_bound()}
            <a class="underline underline-offset-2" href={resolve('media-generation')}>
              {m.route_purpose_open_media()}
            </a>
          {:else}
            {m.backend_error_route_web_search_bound()}
            <a class="underline underline-offset-2" href={resolve('web-search')}>{m.route_purpose_open_search()}</a>
          {/if}
        </p>
      {/if}
      {#if saveError}<p class="mt-3 text-sm text-destructive" role="alert">{saveError}</p>{/if}
    </section>

    <section class="route-section" aria-labelledby="route-billing-title">
      <div class="route-section-header">
        <div class="flex min-w-0 items-center gap-2">
          <h2 id="route-billing-title" class="route-section-title">{m.billing_section()}</h2>
          <Field.Help hint={m.billing_help()} />
        </div>
      </div>
      <Field.Group>
        <Field.Field>
          <Field.FieldLabel id="route-billing-mode-label">{m.billing_mode()}</Field.FieldLabel>
          <SegmentedControl
            id="route-billing-mode"
            options={billingModeOptions}
            value={billingMode}
            onValueChange={(value: BillingMode) => {
              billingMode = value
            }}
            ariaLabelledby="route-billing-mode-label"
            ariaDescribedby="route-billing-mode-help"
            disabled={saving} />
        </Field.Field>
      </Field.Group>
      <p id="route-billing-mode-help" class="mt-3 text-sm text-muted-foreground">{m.billing_charged_to_key_help()}</p>
      {#if billingMode === 'per_call'}
        <div class="mt-4 grid gap-4 @2xl/model-contract:grid-cols-2 @2xl/model-contract:gap-x-6">
          <Field.Field data-invalid={billingAmountIssue ? true : undefined}>
            <Field.FieldLabel for="route-billing-amount">{m.billing_amount()}</Field.FieldLabel>
            <InputGroup.Root>
              <InputGroup.Addon align="inline-start">
                <InputGroup.Text class="font-technical">
                  {currencyForKind(billingCurrency, customCurrency).symbol}
                </InputGroup.Text>
              </InputGroup.Addon>
              <InputGroup.Input
                id="route-billing-amount"
                bind:value={billingAmount}
                inputmode="decimal"
                class="font-technical"
                placeholder="0.00"
                autocomplete="off" />
            </InputGroup.Root>
            <Field.FieldDescription>{m.billing_per_call_help()}</Field.FieldDescription>
            {#if billingAmountIssue && billingAmount.length > 0}
              <Field.FieldError>{billingAmountIssue}</Field.FieldError>
            {/if}
          </Field.Field>
          {#if customCurrency}
            <Field.Field size="setting">
              <Field.FieldLabel for="route-billing-currency">{m.billing_currency()}</Field.FieldLabel>
              <Select.Root
                type="single"
                value={billingCurrency}
                onValueChange={(value: string) => value && (billingCurrency = value as CurrencyKind)}>
                <Select.Trigger id="route-billing-currency" class="w-full" aria-label={m.billing_currency()}>
                  {billingCurrency === 'custom' ? customCurrency.symbol : m.pricing_currency_usd()}
                </Select.Trigger>
                <Select.Content>
                  <Select.Group>
                    <Select.Item value="usd">{m.pricing_currency_usd()}</Select.Item>
                    <Select.Item value="custom">{customCurrency.symbol}</Select.Item>
                  </Select.Group>
                </Select.Content>
              </Select.Root>
            </Field.Field>
          {/if}
        </div>
      {:else if billingMode === 'per_token'}
        <div class="mt-4">
          <PriceEditor bind:value={billingPrice} {customCurrency} idPrefix="route-billing-price" disabled={saving} />
          {#if billingErrors.length > 0}
            <ul class="mt-3 list-disc pl-5 text-sm text-destructive" role="alert">
              {#each billingErrors as issue (issue)}<li>{issue}</li>{/each}
            </ul>
          {/if}
        </div>
      {/if}
    </section>

    <section class="route-section" aria-labelledby="route-targets-title">
      <div class={['route-section-header items-center', targetLanes.length > 0 && 'mb-1']}>
        <div class="flex min-w-0 items-center gap-2">
          <h2 id="route-targets-title" class="route-section-title">{m.model_editor_request_destinations()}</h2>
          {#if savedRouteId}<Field.Help hint={m.model_editor_target_status_refresh_note()} />{/if}
        </div>
        <Button type="button" variant="outline" onclick={addTarget}>
          <CirclePlusIcon data-icon="inline-start" />{m.model_editor_add_destination()}
        </Button>
      </div>

      {#if savedRouteId && targetStatusesFailed}
        <div class="mb-3 flex items-center gap-3">
          <StatusIndicator
            class="min-w-0 flex-1"
            tone="error"
            label={m.model_editor_target_status_unavailable({
              error: localizeBackendErrorMessage(targetStatusesQuery.error),
            })} />
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={targetStatusesQuery.isFetching}
            onclick={() => void targetStatusesQuery.refetch()}>
            {#if targetStatusesQuery.isFetching}<Spinner data-icon="inline-start" />{/if}
            {m.common_retry()}
          </Button>
        </div>
      {/if}

      {#if targets.length > 0}
        <div class="flex flex-col gap-4">
          <div
            data-slot="target-lane-stack"
            class="relative min-w-0"
            role="group"
            aria-label={m.model_editor_enabled_targets()}>
            {#if targetLanes.length === 0}
              <div
                data-slot="target-priority-connector"
                data-position="empty"
                class={[
                  'relative mx-auto flex min-h-16 w-full items-center justify-center rounded-lg border border-dashed transition-[border-color,background-color,color] duration-150 motion-reduce:transition-none',
                  isDragging
                    ? 'border-primary/60 bg-primary/5 text-primary data-[drag-over=true]:border-primary data-[drag-over=true]:bg-primary/20'
                    : 'border-border/80 bg-muted/20 text-muted-foreground',
                ]}
                role="group"
                aria-label={m.model_editor_insert_higher_priority()}>
                <span class="inline-flex items-center gap-2 text-sm">
                  <PlusIcon class="size-4" aria-hidden="true" />
                  {m.model_editor_insert_higher_priority()}
                </span>
              </div>
            {:else}
              {@render priorityConnector({ position: 'top' }, 'top', null, null)}
              <div class="flex flex-col">
                {#each targetLanes as lane, laneIndex (lane.priority)}
                  {@const laneScope = { enabled: true as const, priority: lane.priority }}
                  {@const groups = groupLaneTargetsByModel(lane.targets, laneScope)}
                  <section
                    data-slot="target-lane"
                    data-priority={lane.priority}
                    class={[
                      'overflow-hidden rounded-lg border bg-card/40 transition-[border-color,background-color] duration-150 motion-reduce:transition-none',
                      isDragging
                        ? 'border-dashed border-primary/60 bg-primary/[0.025] data-[drag-over=true]:border-primary data-[drag-over=true]:bg-primary/10'
                        : 'border-border',
                    ]}
                    aria-label={m.model_editor_layer_value({ index: laneIndex + 1 })}>
                    <div class="flex flex-wrap items-center justify-between gap-2 border-b border-border bg-muted/30 px-2.5 py-1">
                      <div class="flex min-w-0 items-center gap-2">
                        <span
                          class={[
                            'inline-flex items-center rounded-md px-2 py-0.5 font-technical text-xs font-medium tabular-nums',
                            laneIndex === 0
                              ? 'bg-primary text-primary-foreground'
                              : 'border border-border bg-background text-foreground',
                          ]}>
                          {m.model_editor_layer_value({ index: laneIndex + 1 })}
                        </span>
                        <span data-slot="target-lane-stats" class="text-xs text-muted-foreground tabular-nums">
                          {layerStatsLabel(groups.length, countDistinctTargetAccounts(lane.targets))}
                        </span>
                      </div>
                      <div class="flex shrink-0 items-center gap-1">
                        {#if laneIndex > 0}
                          <Button
                            type="button"
                            variant="ghost"
                            size="xs"
                            onclick={() =>
                              handleMergeLaneIntoUpper(lane.priority, targetLanes[laneIndex - 1].priority)}>
                            <MergeIcon data-icon="inline-start" />
                            {m.model_editor_layer_merge_upper()}
                          </Button>
                        {/if}
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-xs"
                          disabled={laneIndex === 0}
                          aria-label={m.model_editor_layer_move_up()}
                          title={m.model_editor_layer_move_up()}
                          onclick={() => handleSwapLanes(targetLanes[laneIndex - 1].priority, lane.priority)}>
                          <ArrowUpIcon />
                        </Button>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon-xs"
                          disabled={laneIndex === targetLanes.length - 1}
                          aria-label={m.model_editor_layer_move_down()}
                          title={m.model_editor_layer_move_down()}
                          onclick={() => handleSwapLanes(lane.priority, targetLanes[laneIndex + 1].priority)}>
                          <ArrowDownIcon />
                        </Button>
                      </div>
                    </div>

                    <div class="divide-y divide-border">
                      {#each groups as group (group.model)}
                        {@const groupSummary = groupModelSummary(group)}
                        {@const groupName = groupDisplayName(group, groupSummary)}
                        {@const showTechnicalId = Boolean(group.model.trim() && groupName !== group.model.trim())}
                        {@const showGroupMetaRow = group.targets.length > 1 || group.customTargets.length > 0}
                        {@const isSoloTopGroup = laneIndex === 0 && groups.length === 1}
                        {@const isSoloBottomGroup = laneIndex === targetLanes.length - 1 && groups.length === 1}
                        {@const disableGroupBlocked = enabledTargetCount <= group.targets.length}
                        {@const groupBeingDragged =
                          activeDrag?.kind === 'group' &&
                          activeDrag.keys.every((key) => group.targets.some((member) => member.key === key))}
                        <div
                          data-slot="target-model-group"
                          data-model={group.model}
                          class={['min-w-0 transition-opacity', groupBeingDragged && 'opacity-50']}>
                          <div class="flex flex-col gap-1 border-b border-border/60 px-2.5 py-1.5">
                            <div class="flex flex-wrap items-center justify-between gap-x-2 gap-y-1">
                              <div class="flex min-w-40 flex-1 flex-wrap items-center gap-x-1.5 gap-y-0.5">
                                <span
                                  data-slot="target-group-drag-handle"
                                  role="presentation"
                                  title={m.model_editor_group_drag_hint()}
                                  class="-ml-1 flex size-6 shrink-0 cursor-grab touch-none items-center justify-center rounded text-muted-foreground transition-colors hover:bg-muted hover:text-foreground active:cursor-grabbing"
                                  onpointerdown={(event) => startGroupPointerDrag(event, group)}>
                                  <GripVerticalIcon class="size-3.5" aria-hidden="true" />
                                </span>
                                <span
                                  title={groupName}
                                  class={[
                                    'max-w-full truncate font-medium text-foreground',
                                    !showTechnicalId && 'font-technical text-sm',
                                  ]}>
                                  {groupName}
                                </span>
                                {#if showTechnicalId}
                                  <span class="max-w-full truncate font-technical text-xs text-muted-foreground">
                                    {group.model}
                                  </span>
                                {/if}
                                {#if groupSummary}
                                  <ModelSpecification specification={groupSummary.specification} density="target" />
                                {/if}
                                {#if !showGroupMetaRow}
                                  <button
                                    type="button"
                                    class="inline-block max-w-full truncate whitespace-nowrap rounded px-1 py-0.5 text-left font-technical text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50"
                                    onclick={() => openGroupConfig(laneScope, group.model)}>
                                    {formatGroupConfigSummary(group.sharedConfig)}
                                  </button>
                                {/if}
                              </div>

                              <div class="flex shrink-0 flex-wrap items-center gap-1">
                                <Button
                                  type="button"
                                  variant="outline"
                                  size="xs"
                                  class="h-6 gap-1 px-1.5 text-xs"
                                  onclick={() => openGroupConfig(laneScope, group.model)}>
                                  <SlidersHorizontalIcon data-icon="inline-start" />
                                  {m.model_editor_group_config()}
                                </Button>
                                <Button
                                  type="button"
                                  variant="ghost"
                                  size="xs"
                                  class="h-6 gap-1 px-1.5 text-xs"
                                  disabled={!group.model.trim()}
                                  onclick={() => openBatchAdd(laneScope, group.model)}>
                                  <PlusIcon data-icon="inline-start" />
                                  {m.model_editor_group_add_accounts()}
                                </Button>
                                <Button
                                  type="button"
                                  variant="ghost"
                                  size="icon-xs"
                                  class="size-6"
                                  disabled={isSoloTopGroup}
                                  aria-label={m.model_editor_group_move_up({ model: groupName })}
                                  title={m.model_editor_group_move_up({ model: groupName })}
                                  onclick={() => handleStepGroup(group, 'up')}>
                                  <ArrowUpIcon />
                                </Button>
                                <Button
                                  type="button"
                                  variant="ghost"
                                  size="icon-xs"
                                  class="size-6"
                                  disabled={isSoloBottomGroup}
                                  aria-label={m.model_editor_group_move_down({ model: groupName })}
                                  title={m.model_editor_group_move_down({ model: groupName })}
                                  onclick={() => handleStepGroup(group, 'down')}>
                                  <ArrowDownIcon />
                                </Button>
                                <DropdownMenu.Root>
                                  <DropdownMenu.Trigger>
                                    {#snippet child({ props })}
                                      <Button
                                        type="button"
                                        variant="ghost"
                                        size="icon-xs"
                                        class="size-6"
                                        aria-label={m.model_editor_group_more_actions({ model: groupName })}
                                        {...props}>
                                        <EllipsisIcon />
                                      </Button>
                                    {/snippet}
                                  </DropdownMenu.Trigger>
                                  <DropdownMenu.Content align="end">
                                    <DropdownMenu.Group>
                                      {#each targetLanes as destLane, destIdx (destLane.priority)}
                                        {#if destLane.priority !== lane.priority}
                                          <DropdownMenu.Item
                                            onclick={() => handleMoveGroupToExistingLayer(group, destLane.priority)}>
                                            {m.model_editor_group_move_to_layer({ index: destIdx + 1 })}
                                          </DropdownMenu.Item>
                                        {/if}
                                      {/each}
                                      {#if groups.length > 1}
                                        <DropdownMenu.Item onclick={() => handleSplitGroupToNewLayer(group, laneIndex)}>
                                          {m.model_editor_group_split_layer()}
                                        </DropdownMenu.Item>
                                      {/if}
                                    </DropdownMenu.Group>
                                    {#if targetLanes.length > 1 || groups.length > 1}
                                      <DropdownMenu.Separator />
                                    {/if}
                                    <DropdownMenu.Group>
                                      <DropdownMenu.Item
                                        variant="destructive"
                                        disabled={disableGroupBlocked}
                                        onclick={() => handleDisableGroup(group)}>
                                        {m.model_editor_group_disable()}
                                      </DropdownMenu.Item>
                                    </DropdownMenu.Group>
                                  </DropdownMenu.Content>
                                </DropdownMenu.Root>
                              </div>
                            </div>

                            {#if showGroupMetaRow}
                              <div class="flex flex-wrap items-center gap-x-2 gap-y-1 pl-7 text-xs">
                                <Badge variant="secondary" class="tabular-nums">
                                  {group.targets.length === 1
                                    ? m.model_editor_group_single_account()
                                    : m.model_editor_group_accounts_count({ count: group.targets.length })}
                                </Badge>
                                {#if group.customTargets.length > 0}
                                  <span
                                    class="inline-flex items-center rounded-full border border-warning/40 bg-warning/10 px-2 py-0.5 text-xs font-medium text-warning tabular-nums">
                                    {m.model_editor_group_custom_badge({ count: group.customTargets.length })}
                                  </span>
                                {/if}
                                <button
                                  type="button"
                                  class="inline-block max-w-full truncate whitespace-nowrap rounded px-1 py-0.5 text-left font-technical text-xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50"
                                  onclick={() => openGroupConfig(laneScope, group.model)}>
                                  {formatGroupConfigSummary(group.sharedConfig)}
                                </button>
                              </div>
                            {/if}
                          </div>

                          <div class="flex flex-wrap gap-1.5 px-2 py-1.5">
                            {#each group.targets as target (target.key)}
                              {@const summary = selectedSummary(target)}
                              {@const status = targetRuntimeStatus(target)}
                              {@const mismatch = targetPurposeMismatch(target)}
                              {@const issueLabels = targetIssueLabels(target, summary)}
                              {@const modelIdentity = targetModelIdentity(target, summary)}
                              {@const isCustom = target.configMode === 'custom'}
                              {@const hasError = mismatch || Boolean(status?.credential_invalid) || issueLabels.length > 0}
                              {@const timeoutDiffHint =
                                isCustom &&
                                target.firstTokenTimeoutSeconds !== group.sharedConfig.firstTokenTimeoutSeconds
                                  ? `${target.firstTokenTimeoutSeconds}${m.model_editor_unit_seconds()}`
                                  : ''}
                              <div
                                role="button"
                                tabindex="0"
                                aria-label={m.model_editor_edit_destination_value({ index: targetIndex(target) + 1 })}
                                data-slot="target-card"
                                data-enabled="true"
                                data-key={target.key}
                                data-config-mode={target.configMode}
                                class={[
                                  'group relative inline-flex max-w-full min-w-0 cursor-grab select-none flex-wrap items-center gap-1.5 rounded-md border px-2 py-1 text-sm transition-[border-color,background-color,opacity] hover:border-primary/50 focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 active:cursor-grabbing data-[drag-over=true]:border-primary data-[drag-over=true]:bg-primary/10 motion-reduce:transition-none',
                                  hasError
                                    ? 'border-destructive/50 bg-destructive/[0.04]'
                                    : isCustom
                                      ? 'border-warning/50 bg-warning/[0.04]'
                                      : 'border-border bg-card',
                                  draggedKeys.has(target.key) && 'opacity-50',
                                ]}
                                onpointerdown={(event) => startTargetPointerDrag(event, target, false)}
                                onclick={() => editTarget(target)}
                                onkeydown={(event) => handleTargetCardKeydown(event, target)}>
                                <span
                                  data-slot="target-drag-handle"
                                  role="presentation"
                                  class="-my-1 -ml-1 flex size-7 shrink-0 touch-none items-center justify-center text-muted-foreground"
                                  onpointerdown={(event) => startTargetPointerDrag(event, target, true)}>
                                  <GripVerticalIcon class="size-3.5" aria-hidden="true" />
                                </span>

                                {#if mismatch}
                                  <StatusIndicator compact tone="error" label={m.route_purpose_mismatch()} />
                                {:else if status}
                                  {@const statusLabel = status.credential_invalid
                                    ? m.model_editor_target_status_credential_invalid()
                                    : status.allowance_suspended
                                      ? m.allowances_suspended()
                                      : targetStateLabel(status.state, status.cooldown_remaining_ms)}
                                  {@const statusTone = status.credential_invalid
                                    ? 'error'
                                    : status.allowance_suspended
                                      ? 'warning'
                                      : status.state === 'available'
                                        ? 'healthy'
                                        : 'warning'}
                                  {#if statusTone === 'healthy'}
                                    <span
                                      class="route-status route-status--compact"
                                      data-tone="healthy"
                                      role="status"
                                      title={statusLabel}>
                                      <span class="route-status__mark" aria-hidden="true"></span>
                                      <span class="sr-only">{statusLabel}</span>
                                    </span>
                                  {:else}
                                    <StatusIndicator compact tone={statusTone} label={statusLabel} />
                                  {/if}
                                {/if}

                                <span class="min-w-0 max-w-[14rem] truncate font-medium text-foreground">
                                  {targetProviderLabel(target)}
                                </span>

                                <span class="sr-only">{modelIdentity.label}</span>

                                {#if isCustom}
                                  <span
                                    class="inline-flex shrink-0 items-center gap-1 rounded border border-warning/40 bg-warning/10 px-1.5 py-0.5 text-[11px] font-medium leading-none text-warning">
                                    {m.model_editor_custom_badge()}
                                    {#if timeoutDiffHint}
                                      <span class="font-technical tabular-nums">{timeoutDiffHint}</span>
                                    {/if}
                                  </span>
                                {/if}

                                {#each issueLabels as label (label)}
                                  <Badge variant="destructive">{label}</Badge>
                                {/each}
                              </div>
                            {/each}
                          </div>
                        </div>
                      {/each}
                    </div>
                  </section>

                  {#if laneIndex < targetLanes.length - 1}
                    {@const lowerLane = targetLanes[laneIndex + 1]}
                    {@render priorityConnector(
                      { position: 'between', upperPriority: lane.priority, lowerPriority: lowerLane.priority },
                      `between-${lane.priority}-${lowerLane.priority}`,
                      laneIndex + 1,
                      laneIndex + 2,
                    )}
                  {/if}
                {/each}
              </div>
              {@render priorityConnector({ position: 'bottom' }, 'bottom', null, null)}
            {/if}
          </div>

          <aside
            data-slot="target-dock"
            class={[
              'min-w-0 rounded-lg border border-dashed p-3 transition-[border-color,background-color] duration-150 motion-reduce:transition-none',
              isDragging
                ? 'border-primary/60 bg-primary/5 data-[drag-over=true]:border-primary data-[drag-over=true]:bg-primary/15'
                : 'border-border',
            ]}
            aria-label={m.model_editor_disabled_targets()}>
            <div class={['flex items-center justify-between gap-3 px-1', disabledTargets.length > 0 && 'mb-2.5']}>
              <div class="flex min-w-0 items-center gap-2">
                <h3 class="font-medium">{m.model_editor_disabled_targets()}</h3>
                <Field.Help hint={m.model_editor_disabled_targets_help()} />
              </div>
              <Badge variant="secondary" class="tabular-nums">{disabledTargets.length}</Badge>
            </div>
            {#if disabledTargets.length > 0}
              <div class="flex flex-col gap-2">
                {#each disabledGroups as group (group.model)}
                  {#if group.targets.length >= 2}
                    {@const groupSummary = groupModelSummary(group)}
                    {@const groupName = groupDisplayName(group, groupSummary)}
                    <div
                      data-slot="target-model-group"
                      data-model={group.model}
                      class="flex flex-col gap-1.5 rounded-md border border-border/70 bg-muted/20 p-2">
                      <div class="flex flex-wrap items-center justify-between gap-x-2 gap-y-1 px-0.5">
                        <div class="flex min-w-40 flex-1 items-center gap-1.5 text-xs">
                          <span
                            data-slot="target-group-drag-handle"
                            role="presentation"
                            title={m.model_editor_group_drag_hint()}
                            class="flex size-5 shrink-0 cursor-grab touch-none items-center justify-center rounded text-muted-foreground hover:bg-muted hover:text-foreground active:cursor-grabbing"
                            onpointerdown={(event) => startGroupPointerDrag(event, group)}>
                            <GripVerticalIcon class="size-3.5" aria-hidden="true" />
                          </span>
                          <span title={groupName} class="truncate font-medium text-muted-foreground">{groupName}</span>
                          <Badge variant="secondary" class="shrink-0 tabular-nums">
                            {m.model_editor_group_accounts_count({ count: group.targets.length })}
                          </Badge>
                        </div>
                        <Button
                          type="button"
                          variant="outline"
                          size="xs"
                          class="h-6 gap-1 px-1.5 text-xs"
                          onclick={() => openGroupConfig({ enabled: false }, group.model)}>
                          <SlidersHorizontalIcon data-icon="inline-start" />
                          {m.model_editor_group_config()}
                        </Button>
                      </div>
                      <div class="flex flex-col gap-1.5">
                        {#each group.targets as target (target.key)}
                          {@render disabledTargetCard(target)}
                        {/each}
                      </div>
                    </div>
                  {:else}
                    {#each group.targets as target (target.key)}
                      {@render disabledTargetCard(target)}
                    {/each}
                  {/if}
                {/each}
              </div>
            {/if}
          </aside>
        </div>
      {/if}

      <TargetDetailSheet
        open={targetEditorOpen}
        target={targetEditorTarget}
        index={targetEditorTarget ? targetIndex(targetEditorTarget) : 0}
        isNew={targetEditorIsNew}
        wasInitiallyEnabled={targetEditorInitiallyEnabled}
        purpose={form.purpose}
        providers={availableProviders}
        allProviders={providers}
        otherEnabledTargetCount={targets.filter((candidate) => candidate.enabled && candidate.key !== targetEditorTarget?.key)
          .length}
        groupMemberCount={targetEditorGroupPeers.count}
        sharedPeerConfig={targetEditorGroupPeers.sharedPeerConfig}
        providerLabel={targetProviderLabel}
        modelDisplayName={(candidate: RouteTargetForm) => targetModelIdentity(candidate, selectedSummary(candidate)).label}
        {selectedSummary}
        {modelCandidates}
        thinkingContextFor={targetThinkingContext}
        onOpenChange={(open: boolean) => {
          if (open) {
            targetEditorClosing = false
          } else if (!targetEditorClosing && targetEditorTarget) {
            closeTargetEditor(false)
          }
        }}
        onClose={closeTargetEditor}
        onDelete={deleteEditedTarget}
        onChangeProvider={changeProvider}
        onSelectModel={selectModel}
        onUseInventory={useInventory}
        onResetThinkingRow={resetThinkingRow}
        onRegenerateThinkingMap={regenerateThinkingMap}
        onOpenGroupConfig={() => {
          const current = targetEditorTarget
          if (!current) return
          const scope: RouteTargetGroupScope = current.enabled
            ? { enabled: true, priority: current.priority }
            : { enabled: false }
          const modelId = current.model
          closeTargetEditor(false)
          openGroupConfig(scope, modelId)
        }} />

      <GroupConfigSheet
        open={groupSheet.open && Boolean(activeGroupData)}
        sessionId={groupSheet.sessionId}
        model={groupSheet.model}
        modelDisplayName={activeGroupModelName}
        scope={groupSheet.scope}
        layerNumber={activeGroupLayerNumber}
        members={activeGroupMembers}
        initialConfig={activeGroupData?.sharedConfig ?? {
          firstTokenTimeoutSeconds: 120,
          targetRetryBudget: 5,
          targetCooldownSeconds: 120,
          thinkingLevelMap: [],
        }}
        specification={activeGroupSummary?.specification}
        {savedRouteId}
        providerLabel={targetProviderLabel}
        thinkingContextFor={(candidate: RouteTargetForm) => targetThinkingContext(candidate, selectedSummary(candidate))}
        onClose={() => (groupSheet.open = false)}
        onApply={handleApplyGroupConfig}
        onInspectMember={(member: RouteTargetForm) => editTarget(member)}
        onRegenerateGroup={handleRegenerateGroupThinking}
        onResetGroupRow={handleResetGroupThinkingRow} />

      <BatchAddSheet
        open={batchAddSheet.open}
        sessionId={batchAddSheet.sessionId}
        model={batchAddSheet.model}
        modelDisplayName={batchAddModelName}
        scope={batchAddSheet.scope}
        layerNumber={batchAddLayerNumber}
        purpose={form.purpose}
        providers={availableProviders}
        existingProviderIds={batchAddExistingProviders}
        {knownInventories}
        onClose={() => (batchAddSheet.open = false)}
        onSubmit={handleBatchAddSubmit} />
    </section>

    <section class="route-section" aria-labelledby="route-thinking-title">
      <div class="route-section-header">
        <h2 id="route-thinking-title" class="route-section-title">{m.model_editor_thinking_section()}</h2>
      </div>
      <Field.Group>
        <Field.Field>
          <div class="flex w-fit min-w-0 items-center gap-2">
            <Field.Title>{m.model_editor_thinking_levels()}</Field.Title>
            <Field.Help hint={m.model_editor_thinking_levels_help()} />
          </div>
          <div role="group" aria-label={m.model_editor_thinking_levels()} class="flex flex-wrap gap-1.5">
            {#each THINKING_LEVELS as level (level)}
              {@const blockers = thinkingLevelBlockers(level)}
              {#if !supportedThinkingLevels.includes(level)}
                <Tooltip.Root>
                  <Tooltip.Trigger
                    type="button"
                    data-slot="route-thinking-level"
                    data-level={level}
                    data-supported="false"
                    class="inline-flex cursor-help items-center rounded-md border border-dashed px-2 py-0.5 font-technical text-xs text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50">
                    {level}
                  </Tooltip.Trigger>
                  <Tooltip.Content side="top" sideOffset={8} class="max-w-96 flex-col items-start gap-1.5">
                    <p class="font-medium">{m.model_editor_thinking_blocked_by()}</p>
                    <ul class="flex w-full list-disc flex-col gap-0.5 pl-4 text-left">
                      {#each blockers as blocker (blocker)}
                        <li>{blocker}</li>
                      {/each}
                    </ul>
                  </Tooltip.Content>
                </Tooltip.Root>
              {:else}
                <span
                  data-slot="route-thinking-level"
                  data-level={level}
                  data-supported="true"
                  class="inline-flex items-center gap-1 rounded-md border border-border bg-muted/50 px-2 py-0.5 font-technical text-xs text-foreground">
                  <CheckIcon class="size-3 text-success" aria-hidden="true" />{level}
                </span>
              {/if}
            {/each}
          </div>
        </Field.Field>
        <Field.Field size="setting">
          <Field.Label for="route-default-thinking-level" hint={m.model_editor_default_thinking_level_help()}>
            {m.model_editor_default_thinking_level()}
          </Field.Label>
          <Select.Root type="single" bind:value={form.defaultThinkingLevel}>
            <Select.Trigger
              id="route-default-thinking-level"
              class="w-full"
              aria-label={m.model_editor_default_thinking_level()}>
              {defaultThinkingLevelLabel()}
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
                <Select.Item value={UNSPECIFIED_THINKING_LEVEL}>
                  {m.model_editor_thinking_level_provider_default()}
                </Select.Item>
                {#each supportedThinkingLevels as level (level)}
                  <Select.Item value={level}>{level}</Select.Item>
                {/each}
                {#if defaultThinkingLevelUnsupported}
                  <Select.Item value={form.defaultThinkingLevel}>{form.defaultThinkingLevel}</Select.Item>
                {/if}
              </Select.Group>
            </Select.Content>
          </Select.Root>
        </Field.Field>
      </Field.Group>
      {#if defaultThinkingLevelUnsupported}
        <p data-slot="default-thinking-level-warning" class="mt-2 text-sm text-destructive">
          {m.model_editor_default_thinking_level_unavailable({ level: form.defaultThinkingLevel })}
        </p>
      {/if}
    </section>

    <div
      data-slot="model-editor-footer"
      class="sticky bottom-0 z-20 mt-auto translate-y-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
      <div class="flex flex-wrap items-center justify-end gap-2">
        <Button href="/models" variant="outline">{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_model()}
        </Button>
      </div>
    </div>
  </form>
</div>

<AlertDialog.Root bind:open={leaveConfirmOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.model_editor_discard_unsaved_changes()}</AlertDialog.Title>
      <AlertDialog.Description>{m.model_editor_unsaved_changes_warning()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={keepEditing}>{m.model_editor_keep_editing()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" onclick={() => void discardDraftAndLeave()}>
        {m.model_editor_discard_changes()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
