<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { beforeNavigate, goto } from '$app/navigation'
import { base, resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import CirclePlusIcon from '@lucide/svelte/icons/circle-plus'
import GripVerticalIcon from '@lucide/svelte/icons/grip-vertical'
import PlusIcon from '@lucide/svelte/icons/plus'
import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import WaypointsIcon from '@lucide/svelte/icons/waypoints'
import { tick, untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { modelIdFromCatalogId } from '$lib/catalog-model-id'
import { localizeBackendErrorMessage, unrepresentableThinkingTarget } from '$lib/backend-error'
import { formatList } from '$lib/format'
import { inputValue } from '$lib/utils.js'
import {
  thinkingControlContext,
  thinkingControlWritable,
  unrepresentableThinkingLevels,
  writableThinkingControlKinds,
  type ThinkingControlKind,
} from '$lib/thinking-control'
import type {
  Provider,
  ProviderModelSummary,
  Route,
  RouteSelectionStrategy,
  TargetRuntimeStatus,
  TargetThinkingControl,
  ThinkingLevel,
  ThinkingLevelMapping,
  ProviderDescriptor,
} from '$lib/types'
import {
  addRouteTarget,
  buildRouteTargets,
  createRouteTargetForms,
  moveRouteTargetToDock,
  moveRouteTargetToInsertion,
  moveRouteTargetToLane,
  planRouteTargetInsertion,
  priorityLanes,
  removeRouteTarget,
  reorderRouteTargetBefore,
  routeSupportedThinkingLevels,
  type RouteTargetForm,
  type RouteTargetInsertion,
} from './route-targets-form.js'
import ModelCombobox from '$lib/components/model-combobox.svelte'
import ModelIdCombobox from '$lib/components/model-id-combobox.svelte'
import ModelDetailsDialog from '$lib/components/model-details-dialog.svelte'
import ModelSpecification from '$lib/components/model-specification.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import * as Tooltip from '$lib/components/ui/tooltip'

interface Props {
  model?: Route
  providers: Provider[]
  initialProviderId?: string
  initialModelId?: string
  onSaved?: () => void
}

const thinkingLevels: ThinkingLevel[] = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']
const UNSPECIFIED_THINKING_LEVEL = 'unspecified'

let { model, providers, initialProviderId = '', initialModelId = '', onSaved }: Props = $props()
const initialModel = untrack(() => model)
const queryClient = useQueryClient()
let form = $state({
  modelId: initialModel?.model_id ?? '',
  displayName: initialModel?.display_name ?? '',
  balance: initialModel?.balance ?? 'traffic_equalization',
  enabled: initialModel?.is_enabled ?? true,
  defaultThinkingLevel: initialModel?.default_thinking_level ?? UNSPECIFIED_THINKING_LEVEL,
})
let targets = $state<RouteTargetForm[]>(
  untrack(() => createRouteTargetForms(initialModel, initialProviderId, initialModelId)),
)
let saving = $state(false)
let initialized = $state(false)
let regenerateOpen = $state(false)
let regenerateTarget = $state<RouteTargetForm>()
let targetEditorOpen = $state(false)
let targetEditorTarget = $state<RouteTargetForm>()
let targetEditorIsNew = $state(false)
let targetEditorClosing = false
let draggedTargetKey = $state('')
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
    targets: targets
      .map((target) => draftTarget(editedTarget?.key === target.key ? editedTarget : target))
      .toSorted((left, right) => left.key.localeCompare(right.key)),
  }
}

let savedDraft = $state.raw(untrack(draftSnapshot))
const draftChanged = $derived(JSON.stringify(draftSnapshot()) !== JSON.stringify(savedDraft))

beforeNavigate((navigation) => {
  if (!draftChanged) return
  navigation.cancel()
  if (navigation.willUnload || !navigation.to?.url) return

  const { pathname, search, hash } = navigation.to.url
  const href = `/${pathname.slice(base.length + 1)}${search}${hash}` as const
  pendingNavigation = navigation.type === 'popstate' ? () => history.go(navigation.delta) : () => goto(resolve(href))
  leaveConfirmOpen = true
})

const availableProviders = $derived(providers)
const targetLanes = $derived(priorityLanes(targets))
const disabledTargets = $derived(targets.filter((target) => !target.enabled))
const isDraggingTarget = $derived(Boolean(draggedTargetKey))
const canonicalModelsQuery = createQuery(() => ({
  queryKey: ['catalog', 'canonical-models'],
  queryFn: () => admin.catalog.canonicalModels(),
}))
const canonicalModels = $derived(canonicalModelsQuery.data?.models ?? [])
const providerDescriptorsQuery = createQuery(() => ({
  queryKey: ['provider-descriptors'],
  queryFn: admin.providers.descriptors,
  // 能力在按需打开的下拉菜单中读取，不能让初始错误标志的属性跟踪漏掉成功快照。
  notifyOnChangeProps: 'all',
}))
const providerDescriptors = $derived<ProviderDescriptor[]>(providerDescriptorsQuery.data ?? [])

function providerOnlySearchSupported(providerId: string): boolean {
  if (!providerDescriptorsQuery.isSuccess) return false
  const provider = providers.find((candidate) => candidate.id === providerId)
  if (!provider?.vendor) return false
  const descriptor = providerDescriptors.find((candidate) => candidate.provider_id === provider.vendor)
  const channelId = provider.channel ?? 'default'
  const channel = descriptor?.channels.find((candidate) => candidate.id === channelId)
  return Boolean(channel && channel.capabilities.includes('search') && !channel.search_model_required)
}

function providerOnlySearchUnavailable(target: RouteTargetForm): boolean {
  return target.model === null && providerDescriptorsQuery.isSuccess && !providerOnlySearchSupported(target.providerId)
}

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
  // A failed refresh must not keep showing stale status as if it were current.
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
  const model = target.model === null ? null : target.model.trim()
  if (!status || status.provider_id !== target.providerId || status.model !== model) return undefined
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

function strategyLabel(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'traffic_equalization':
      return m.model_editor_traffic_equalization()
    case 'latency_preference':
      return m.model_editor_latency_preference()
  }
}

function strategyHelp(strategy: RouteSelectionStrategy): string {
  switch (strategy) {
    case 'traffic_equalization':
      return m.model_editor_traffic_equalization_help()
    case 'latency_preference':
      return m.model_editor_latency_preference_help()
  }
}

function strategySummary(strategy: RouteSelectionStrategy): string {
  return strategyLabel(strategy)
}

$effect(() => {
  if (!initialized && providers.length > 0) {
    initialized = true
    for (const target of targets) {
      if (target.providerId && target.model !== null) void loadInventory(target, true)
    }
  }
})

function modelCandidates(target: RouteTargetForm): ProviderModelSummary[] {
  return target.inventory.filter((item) => {
    const keepExisting = target.persisted && item.id === target.model
    return item.available || keepExisting
  })
}

function selectedSummary(target: RouteTargetForm): ProviderModelSummary | undefined {
  return target.inventory.find((item) => item.id === target.model)
}

async function loadInventory(target: RouteTargetForm, initializeDraft = false): Promise<void> {
  if (!target.providerId || target.model === null) return
  target.loading = true
  target.validationError = ''
  try {
    target.inventory = (await admin.providers.models(target.providerId)).models
    if (target.model) {
      const summary = selectedSummary(target)
      target.custom = !summary
      if (summary && !target.persisted) {
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
  target.inventory = []
  target.custom = false
  target.persisted = false
  target.validationError = ''
  target.thinkingLevelMap = []
  await loadInventory(target)
}

async function changeDestinationType(target: RouteTargetForm, type: string): Promise<void> {
  if (type === 'provider_only') {
    target.model = null
    target.custom = false
    target.validationError = ''
    target.thinkingLevelMap = []
    return
  }
  if (target.model === null) {
    target.model = ''
    target.validationError = ''
    await loadInventory(target)
  }
}

async function selectModel(target: RouteTargetForm, modelId: string): Promise<void> {
  target.model = modelId
  target.custom = false
  target.validationError = ''
  target.thinkingLevelMap = []
  await loadThinkingMap(target)
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
  targetEditorClosing = false
  targetEditorTarget = cloneTarget(target)
  targetEditorIsNew = isNew
  targetEditorOpen = true
  if (targetEditorTarget.providerId && targetEditorTarget.inventory.length === 0) void loadInventory(targetEditorTarget)
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
      Object.assign(targets[index], cloneTarget(target))
    } else if (!save && targetEditorIsNew && index >= 0) {
      removeRouteTarget(targets, index)
    }
  }
  targetEditorTarget = undefined
  targetEditorIsNew = false
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

function targetConfigured(target: RouteTargetForm): boolean {
  return Boolean(target.providerId && (target.model === null || target.model.trim()))
}

function startTargetDrag(event: DragEvent, target: RouteTargetForm): void {
  draggedTargetKey = target.key
  event.dataTransfer?.setData('text/plain', target.key)
  if (event.dataTransfer) event.dataTransfer.effectAllowed = 'all'
}

function finishTargetDrag(): void {
  // 部分浏览器会先派发 dragend 再 drop；延后清空以免空层 drop 丢失拖拽源。
  setTimeout(() => {
    draggedTargetKey = ''
  }, 0)
}

function allowTargetDrop(event: DragEvent): void {
  event.preventDefault()
  if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy'
}

function draggedKey(event: DragEvent): string {
  return draggedTargetKey || event.dataTransfer?.getData('text/plain') || ''
}

async function dropOnLane(event: DragEvent, priority: number, beforeKey?: string): Promise<void> {
  event.preventDefault()
  event.stopPropagation()
  const key = draggedKey(event)
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return
  if (beforeKey && target.enabled && target.priority === priority) {
    reorderRouteTargetBefore(targets, key, beforeKey)
    draggedTargetKey = ''
    return
  }
  if (!target.enabled && (await blockEnableForUnwritableThinking(target))) return
  if (!moveRouteTargetToLane(targets, key, priority)) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  draggedTargetKey = ''
}

async function dropOnInsertion(event: DragEvent, insertion: RouteTargetInsertion): Promise<void> {
  event.preventDefault()
  event.stopPropagation()
  const key = draggedKey(event)
  const target = targets.find((candidate) => candidate.key === key)
  if (!target) return
  if (!targetConfigured(target)) {
    toast.error(m.model_editor_complete_target_before_enabling())
    return
  }
  if (!target.enabled && (await blockEnableForUnwritableThinking(target))) return
  if (!moveRouteTargetToInsertion(targets, key, insertion)) {
    toast.error(m.model_editor_cannot_create_priority_layer())
    return
  }
  draggedTargetKey = ''
}

function dropInDock(event: DragEvent): void {
  event.preventDefault()
  const key = draggedKey(event)
  if (!moveRouteTargetToDock(targets, key)) {
    toast.error(m.model_editor_last_enabled_target_required())
    return
  }
  draggedTargetKey = ''
}

function insertionLabel(insertion: RouteTargetInsertion): string {
  if (insertion.position === 'top') return m.model_editor_insert_higher_priority()
  if (insertion.position === 'bottom') return m.model_editor_insert_lower_priority()
  const priority = planRouteTargetInsertion(targets, draggedTargetKey, insertion)?.priority
  return priority === undefined
    ? m.model_editor_insert_new_priority()
    : m.model_editor_insert_priority_value({ priority })
}

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
  const model = target.model === null ? m.model_editor_provider_only_search_destination() : target.model.trim()
  return [destination, provider?.name ?? target.providerId, model].filter(Boolean).join(' · ')
}

function thinkingLevelBlockers(level: ThinkingLevel): string[] {
  return targets.flatMap((target, index) =>
    !target.enabled ? [] : targetSupportsThinkingLevel(target, level) ? [] : [targetLabel(target, index)],
  )
}

function changeThinkingControlKind(row: ThinkingLevelMapping, type: TargetThinkingControl['type']): void {
  row.control =
    type === 'effort'
      ? { type, value: row.level === 'off' ? 'none' : row.level }
      : type === 'budget'
        ? { type, value: 1024 }
        : { type }
  row.source = 'overridden'
  if (targetEditorTarget) targetEditorTarget.validationError = ''
}

function changeThinkingControlValue(row: ThinkingLevelMapping, value: string | number): void {
  if (row.control.type === 'effort') row.control.value = String(value)
  if (row.control.type === 'budget') row.control.value = Math.max(0, Math.trunc(Number(value) || 0))
  row.source = 'overridden'
}

function thinkingControlLabel(type: TargetThinkingControl['type']): string {
  return {
    effort: m.model_editor_thinking_effort(),
    budget: m.model_editor_thinking_budget(),
    enabled: m.model_editor_thinking_enabled(),
    disabled: m.model_editor_thinking_disabled(),
    hidden: m.model_editor_thinking_hidden(),
  }[type]
}

function targetThinkingContext(target: RouteTargetForm) {
  return thinkingControlContext(
    providers.find((provider) => provider.id === target.providerId),
    target.model ?? '',
  )
}

function unwritableThinkingLevels(target: RouteTargetForm): ThinkingLevel[] {
  return unrepresentableThinkingLevels(target.thinkingLevelMap, targetThinkingContext(target))
}

function thinkingRowWritable(target: RouteTargetForm, row: ThinkingLevelMapping): boolean {
  return thinkingControlWritable(row.control, targetThinkingContext(target))
}

function thinkingControlOptions(target: RouteTargetForm, row: ThinkingLevelMapping): ThinkingControlKind[] {
  const writable = writableThinkingControlKinds(targetThinkingContext(target))
  return writable.includes(row.control.type) ? writable : [row.control.type, ...writable]
}

function thinkingRowHint(target: RouteTargetForm, row: ThinkingLevelMapping): string {
  return m.model_editor_thinking_row_unwritable({
    control: thinkingControlLabel(row.control.type),
    supported: formatList(writableThinkingControlKinds(targetThinkingContext(target)).map(thinkingControlLabel)),
  })
}

async function blockEnableForUnwritableThinking(target: RouteTargetForm): Promise<boolean> {
  if (target.thinkingLevelMap.length === 0 && target.providerId && target.model?.trim()) {
    await loadThinkingMap(target)
  }
  const levels = unwritableThinkingLevels(target)
  if (levels.length === 0) return false
  toast.error(m.model_editor_thinking_enable_blocked({ levels: formatList(levels) }))
  draggedTargetKey = ''
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

function requestThinkingMapRegeneration(target: RouteTargetForm): void {
  regenerateTarget = target
  regenerateOpen = true
}

async function regenerateThinkingMap(): Promise<void> {
  const target = regenerateTarget
  if (!initialModel || !target?.id) return

  regenerateOpen = false
  target.validationError = ''
  try {
    const updated = await admin.models.regenerateThinkingMap(initialModel.model_id, target.id)
    acceptImmediateThinkingMap(
      target,
      updated.targets.find((candidate) => candidate.id === target.id)?.thinking_level_map ?? target.thinkingLevelMap,
    )
  } catch (error) {
    target.validationError = localizeBackendErrorMessage(error)
  } finally {
    regenerateTarget = undefined
  }
}

async function saveModel(): Promise<void> {
  const result = buildRouteTargets(targets)
  if (result.error && result.error !== 'incomplete-target' && result.error !== 'no-enabled-target') {
    toast.error(m.model_editor_invalid_target_controls())
    return
  }
  const cleanTargets = result.targets
  const firstTarget = cleanTargets.find((target) => target.enabled)
  if (
    !form.modelId.trim() ||
    !firstTarget ||
    result.error === 'incomplete-target' ||
    result.error === 'no-enabled-target'
  ) {
    toast.error(m.model_editor_enabled_destination_required())
    return
  }

  const blocked = targets.find((target) => target.enabled && unwritableThinkingLevels(target).length > 0)
  if (blocked) {
    toast.error(m.model_editor_thinking_enable_blocked({ levels: formatList(unwritableThinkingLevels(blocked)) }))
    editTarget(blocked)
    return
  }

  saving = true
  try {
    for (const target of targets) {
      if (target.model === null) continue
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

    const input = {
      model_id: form.modelId.trim(),
      display_name: form.displayName.trim(),
      balance: form.balance,
      target_provider: firstTarget.provider_id,
      target_model: firstTarget.model,
      targets: cleanTargets,
      default_thinking_level:
        form.defaultThinkingLevel === UNSPECIFIED_THINKING_LEVEL ? null : (form.defaultThinkingLevel as ThinkingLevel),
    }
    if (initialModel) {
      await admin.models.update(initialModel.model_id, { ...input, is_enabled: form.enabled })
    } else {
      const created = await admin.models.create(input)
      if (!form.enabled) {
        try {
          await admin.models.update(created.model_id, { is_enabled: false })
        } catch {
          await queryClient.invalidateQueries({ queryKey: ['models'] })
          toast.error(m.model_editor_model_was_added_but_not_disabled_review_status())
          savedDraft = draftSnapshot()
          await tick()
          await goto(resolve('/models/[id]', { id: created.model_id }))
          return
        }
      }
    }
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['models'] }),
      queryClient.invalidateQueries({ queryKey: ['api-keys'] }),
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
    ])
    savedDraft = draftSnapshot()
    await tick()
    toast.success(m.model_editor_model_saved())
    onSaved?.()
    if (!onSaved) await goto(resolve('/models'))
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
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

{#snippet priorityConnector(insertion: RouteTargetInsertion, position: string)}
  <div
    data-slot="target-priority-connector"
    data-position={position}
    data-upper-priority={insertion.position === 'between' ? insertion.upperPriority : undefined}
    data-lower-priority={insertion.position === 'between' ? insertion.lowerPriority : undefined}
    class={[
      'relative mx-auto flex w-full items-center justify-center overflow-hidden transition-[height,border-color,background-color,color] duration-150 motion-reduce:transition-none',
      isDraggingTarget
        ? 'h-10 rounded-lg border border-dashed border-primary/60 bg-primary/5 text-primary'
        : 'h-5 border border-transparent text-transparent',
    ]}
    role="group"
    aria-label={insertionLabel(insertion)}
    ondragenter={allowTargetDrop}
    ondragover={allowTargetDrop}
    ondrop={(event) => dropOnInsertion(event, insertion)}>
    <span
      class={[
        'pointer-events-none absolute left-1/2 top-0 h-full w-px -translate-x-1/2',
        isDraggingTarget ? 'bg-primary/50' : 'bg-border',
      ]}
      aria-hidden="true"></span>
    {#if isDraggingTarget}
      <span class="relative inline-flex items-center gap-2 rounded-md bg-background/90 px-3 py-1 text-sm shadow-xs">
        <PlusIcon class="size-4" aria-hidden="true" />
        {insertionLabel(insertion)}
      </span>
    {/if}
  </div>
{/snippet}

<div class="route-page mx-auto min-h-[calc(100svh-5rem)] w-full max-w-[90rem]">
  <PageHeader
    eyebrow={m.common_setup()}
    title={initialModel ? m.model_editor_edit_model() : m.common_add_model()}
    description={m.model_editor_choose_model_name_apps_use_where_stravia_send()} />

  <form
    class="flex flex-1 flex-col gap-6"
    onsubmit={(event) => {
      event.preventDefault()
      void saveModel()
    }}>
    <section class="route-section" aria-labelledby="route-contract-title">
      <div class="route-section-header max-sm:flex-col">
        <div class="min-w-0 flex-1">
          <h2 id="route-contract-title" class="route-section-title">{m.model_editor_client_model()}</h2>
          <p class="route-section-description">
            {m.model_editor_set_model_name_request_type_client_apps_use()}
          </p>
        </div>
        <div data-slot="model-enabled-control" class="flex min-h-10 shrink-0 items-center gap-2">
          <Field.Label for="route-enabled">{m.common_enable_action()}</Field.Label>
          <Switch
            id="route-enabled"
            aria-label={m.common_enable_action()}
            bind:checked={() => form.enabled, (checked: boolean) => (form.enabled = checked)} />
        </div>
      </div>
      <Field.Group class="grid gap-4 md:grid-cols-4">
        <Field.Field orientation="vertical" class="md:col-span-2">
          <Field.Label for="route-model-id">{m.model_editor_model_id()}</Field.Label>
          <ModelIdCombobox
            id="route-model-id"
            value={form.modelId}
            models={canonicalModels}
            placeholder={m.model_editor_model_id_placeholder()}
            emptyText={m.model_editor_no_models_found()}
            ariaLabel={m.model_editor_model_id()}
            clearAriaLabel={m.model_editor_clear_selected_model()}
            onInput={(value: string) => {
              form.modelId = value
            }}
            onSelect={(model: { id: string; name: string }) => {
              form.modelId = modelIdFromCatalogId(model.id)
              form.displayName = model.name
            }}
            onClear={() => {
              form.modelId = ''
              form.displayName = ''
            }} />
        </Field.Field>
        <Field.Field orientation="vertical">
          <Field.Label for="route-display-name">{m.model_editor_model_name()}</Field.Label>
          <Input
            id="route-display-name"
            bind:value={form.displayName}
            placeholder={m.model_editor_model_name_placeholder()} />
        </Field.Field>
        <Field.Field orientation="vertical">
          <Field.Label for="route-balance">{m.model_editor_how_requests_sent()}</Field.Label>
          <Select.Root type="single" bind:value={form.balance}>
            <Select.Trigger id="route-balance" class="w-full" aria-label={m.model_editor_how_requests_sent()}>
              {strategyLabel(form.balance)}
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
                <Select.Item value="traffic_equalization">{m.model_editor_traffic_equalization()}</Select.Item>
                <Select.Item value="latency_preference">{m.model_editor_latency_preference()}</Select.Item>
              </Select.Group>
            </Select.Content>
          </Select.Root>
        </Field.Field>
      </Field.Group>
    </section>

    <section class="route-section" aria-labelledby="route-targets-title">
      <div class="route-section-header">
        <div>
          <h2 id="route-targets-title" class="route-section-title">{m.model_editor_request_destinations()}</h2>
          <p class="route-section-description">{strategyHelp(form.balance)}</p>
        </div>
        <Button type="button" variant="outline" onclick={addTarget}
          ><CirclePlusIcon data-icon="inline-start" />{m.model_editor_add_destination()}</Button>
      </div>

      {#if providerDescriptorsQuery.isError}
        <RequestFailure
          class="mb-4"
          title={m.provider_config_plugins_load_failed()}
          message={localizeBackendErrorMessage(providerDescriptorsQuery.error)}
          retry={() => providerDescriptorsQuery.refetch()}
          retrying={providerDescriptorsQuery.isFetching} />
      {/if}

      {#if savedRouteId}
        <p class="mb-2 mt-3 text-xs text-muted-foreground">
          {m.model_editor_target_status_refresh_note()}
        </p>
        {#if targetStatusesFailed}
          <div class="mb-2 mt-3 flex items-center gap-3">
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
      {/if}

      <div class="grid gap-4 lg:grid-cols-[minmax(0,3fr)_minmax(16rem,1fr)]">
        <div
          data-slot="target-lane-stack"
          class="relative min-w-0 px-1 sm:px-4"
          role="group"
          aria-label={m.model_editor_enabled_targets()}
          ondragenter={targetLanes.length === 0 ? allowTargetDrop : undefined}
          ondragover={targetLanes.length === 0 ? allowTargetDrop : undefined}
          ondrop={targetLanes.length === 0 ? (event) => dropOnInsertion(event, { position: 'top' }) : undefined}>
          <div
            data-slot="target-entry-model"
            class={[
              'mx-auto flex min-h-16 w-fit min-w-64 max-w-full items-center gap-3 rounded-xl bg-background px-4 py-3 shadow-sm ring-1 ring-border',
              isDraggingTarget && 'pointer-events-none',
            ]}>
            <span class="flex size-9 shrink-0 items-center justify-center rounded-full bg-primary/10 text-primary">
              <WaypointsIcon class="size-4" aria-hidden="true" />
            </span>
            <span class="min-w-0">
              <span class="block text-xs text-muted-foreground">{m.model_editor_entry_model()}</span>
              <span class="block truncate font-technical text-sm">
                {form.modelId.trim() || m.model_editor_model_id_placeholder()}
              </span>
            </span>
          </div>

          {#if targetLanes.length === 0}
            <div
              data-slot="target-priority-connector"
              data-position="empty"
              class={[
                'relative mx-auto flex min-h-24 w-full items-center justify-center rounded-xl border border-dashed transition-[border-color,background-color,color] duration-150 motion-reduce:transition-none',
                isDraggingTarget
                  ? 'border-primary/60 bg-primary/5 text-primary'
                  : 'border-border/80 bg-muted/20 text-muted-foreground',
              ]}
              role="group"
              aria-label={m.model_editor_insert_higher_priority()}
              ondragenter={allowTargetDrop}
              ondragover={allowTargetDrop}
              ondrop={(event) => dropOnInsertion(event, { position: 'top' })}>
              <span class="inline-flex items-center gap-2 text-sm">
                <PlusIcon class="size-4" aria-hidden="true" />
                {m.model_editor_insert_higher_priority()}
              </span>
            </div>
          {:else}
            {@render priorityConnector({ position: 'top' }, 'top')}
            <div class="flex flex-col">
              {#each targetLanes as lane, laneIndex (lane.priority)}
                <section
                  data-slot="target-lane"
                  data-priority={lane.priority}
                  class={[
                    'grid min-h-24 grid-cols-[3.5rem_minmax(0,1fr)] overflow-hidden rounded-xl border bg-background shadow-sm transition-[border-color,box-shadow,background-color] duration-150 motion-reduce:transition-none',
                    isDraggingTarget
                      ? 'border-dashed border-primary/60 bg-primary/[0.025]'
                      : 'border-transparent ring-1 ring-border',
                  ]}
                  aria-label={m.model_editor_layer_value({ index: laneIndex + 1 })}
                  ondragenter={allowTargetDrop}
                  ondragover={allowTargetDrop}
                  ondrop={(event) => dropOnLane(event, lane.priority)}>
                  <div class="flex flex-col items-center justify-center bg-muted/55 px-2 text-center">
                    <span class="font-technical text-[0.65rem] uppercase tracking-[0.14em] text-muted-foreground">
                      {m.model_editor_layer()}
                    </span>
                    <strong class="font-technical text-lg tabular-nums">{laneIndex + 1}</strong>
                  </div>
                  <div class="flex min-w-0 flex-wrap content-start items-stretch gap-2 p-2">
                    {#each lane.targets as target (target.key)}
                      {@const summary = selectedSummary(target)}
                      {@const status = targetRuntimeStatus(target)}
                      <div
                        role="button"
                        tabindex="0"
                        aria-label={m.model_editor_edit_destination_value({ index: targetIndex(target) + 1 })}
                        data-slot="target-card"
                        data-enabled="true"
                        data-key={target.key}
                        draggable="true"
                        class={[
                          'group @container/target flex min-h-20 min-w-0 flex-[1_1_15rem] cursor-grab select-none flex-col items-start rounded-lg bg-card p-3 text-left shadow-xs ring-1 ring-border transition-opacity focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 active:cursor-grabbing motion-reduce:transition-none',
                          draggedTargetKey === target.key && 'opacity-50',
                        ]}
                        onclick={() => editTarget(target)}
                        onkeydown={(event) => {
                          if (event.target === event.currentTarget && (event.key === 'Enter' || event.key === ' ')) {
                            event.preventDefault()
                            editTarget(target)
                          }
                        }}
                        ondragstart={(event) => startTargetDrag(event, target)}
                        ondragend={finishTargetDrag}
                        ondragenter={allowTargetDrop}
                        ondragover={allowTargetDrop}
                        ondrop={(event) => dropOnLane(event, lane.priority, target.key)}>
                        <div
                          class="grid w-full min-w-0 grid-cols-[auto_minmax(0,1fr)_minmax(0,1fr)_auto] items-center gap-x-2 gap-y-1 text-left @max-md/target:grid-cols-[auto_minmax(0,1fr)_minmax(0,1fr)]">
                          <GripVerticalIcon class="size-4 shrink-0 text-muted-foreground" />
                          <span class="truncate font-medium">
                            {providers.find((provider) => provider.id === target.providerId)?.name ?? target.providerId}
                          </span>
                          <span class="min-w-0 flex-1 truncate font-technical text-sm text-muted-foreground">
                            {target.model ?? m.model_editor_provider_only_search_destination()}
                          </span>
                          {#if status}
                            <StatusIndicator
                              compact
                              class="@max-md/target:col-span-2 @max-md/target:col-start-2"
                              label={targetStateLabel(status.state, status.cooldown_remaining_ms)}
                              tone={status.state === 'available' ? 'healthy' : 'warning'} />
                          {/if}
                        </div>
                        <div class="mt-auto flex flex-wrap gap-1.5 pl-6 pt-2">
                          {#if target.persisted && summary && !summary.available}
                            <Badge variant="destructive">{m.model_editor_model_no_longer_available()}</Badge>
                          {/if}
                          {#if providerOnlySearchUnavailable(target)}
                            <Badge variant="destructive">{m.model_editor_provider_only_search_unavailable()}</Badge>
                          {/if}
                          {#if unwritableThinkingLevels(target).length > 0}
                            <Badge variant="destructive">{m.model_editor_thinking_map_unwritable()}</Badge>
                          {/if}
                          {#if summary}<ModelSpecification specification={summary.specification} />{/if}
                        </div>
                      </div>
                    {/each}
                    {#if isDraggingTarget}
                      <span class="ml-auto flex min-h-10 items-center px-3 text-sm text-primary">
                        {m.model_editor_drop_same_priority()}
                      </span>
                    {/if}
                  </div>
                </section>

                {#if laneIndex < targetLanes.length - 1}
                  {@const lowerLane = targetLanes[laneIndex + 1]}
                  {@render priorityConnector(
                    { position: 'between', upperPriority: lane.priority, lowerPriority: lowerLane.priority },
                    `between-${lane.priority}-${lowerLane.priority}`,
                  )}
                {/if}
              {/each}
            </div>
            {@render priorityConnector({ position: 'bottom' }, 'bottom')}
          {/if}

          <div class={['flex flex-col items-center', isDraggingTarget && 'pointer-events-none']}>
            <span class="size-2.5 rounded-full bg-primary ring-4 ring-primary/10"></span>
            <span class="mt-1.5 text-xs text-muted-foreground">{m.model_editor_end()}</span>
          </div>
        </div>

        <aside
          data-slot="target-dock"
          class="min-w-0 rounded-2xl border border-dashed bg-muted/30 p-3"
          aria-label={m.model_editor_disabled_targets()}
          ondragenter={allowTargetDrop}
          ondragover={allowTargetDrop}
          ondrop={dropInDock}>
          <div class="mb-3 flex items-center justify-between gap-3 px-1">
            <div>
              <h3 class="font-medium">{m.model_editor_disabled_targets()}</h3>
              <p class="text-sm text-muted-foreground">{m.model_editor_disabled_targets_help()}</p>
            </div>
            <Badge variant="secondary">{disabledTargets.length}</Badge>
          </div>
          <div class="flex flex-col gap-2">
            {#each disabledTargets as target (target.key)}
              {@const summary = selectedSummary(target)}
              <div class="relative">
                <div
                  role="button"
                  tabindex="0"
                  aria-label={m.model_editor_edit_destination_value({ index: targetIndex(target) + 1 })}
                  data-slot="target-card"
                  data-key={target.key}
                  draggable="true"
                  class={[
                    'group flex min-h-20 w-full cursor-grab select-none flex-col items-start rounded-lg border bg-background p-3 pr-12 text-left shadow-xs transition-[border-color,opacity] hover:border-primary/40 focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 active:cursor-grabbing',
                    draggedTargetKey === target.key && 'opacity-50',
                  ]}
                  onclick={() => editTarget(target)}
                  onkeydown={(event) => {
                    if (event.target === event.currentTarget && (event.key === 'Enter' || event.key === ' ')) {
                      event.preventDefault()
                      editTarget(target)
                    }
                  }}
                  ondragstart={(event) => startTargetDrag(event, target)}
                  ondragend={finishTargetDrag}>
                  <div class="flex w-full min-w-0 flex-col items-start text-left">
                    <span class="flex w-full min-w-0 items-center gap-2">
                      <GripVerticalIcon class="size-4 shrink-0 text-muted-foreground" />
                      <span class="truncate font-medium">
                        {providers.find((provider) => provider.id === target.providerId)?.name ||
                          target.providerId ||
                          m.model_editor_unconfigured_target()}
                      </span>
                    </span>
                    <span class="mt-1 w-full truncate pl-6 font-technical text-sm text-muted-foreground">
                      {target.model === null
                        ? m.model_editor_provider_only_search_destination()
                        : target.model || m.model_editor_choose_model()}
                    </span>
                  </div>
                  <div class="mt-auto flex flex-wrap gap-1.5 pl-6 pt-2">
                    {#if target.persisted && summary && !summary.available}
                      <Badge variant="destructive">{m.model_editor_model_no_longer_available()}</Badge>
                    {/if}
                    {#if providerOnlySearchUnavailable(target)}
                      <Badge variant="destructive">{m.model_editor_provider_only_search_unavailable()}</Badge>
                    {/if}
                    {#if unwritableThinkingLevels(target).length > 0}
                      <Badge variant="destructive">{m.model_editor_thinking_map_unwritable()}</Badge>
                    {/if}
                    {#if summary}<ModelSpecification specification={summary.specification} />{/if}
                  </div>
                </div>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  class="absolute right-1.5 top-1.5 size-9"
                  aria-label={m.model_editor_remove_destination_value({ index: targetIndex(target) + 1 })}
                  onclick={() => removeDisabledTarget(target)}>
                  <Trash2Icon />
                </Button>
              </div>
            {:else}
              <div
                class="flex min-h-28 items-center justify-center rounded-xl border border-dashed bg-background/50 px-4 text-center text-sm text-muted-foreground">
                {m.model_editor_no_disabled_targets()}
              </div>
            {/each}
          </div>
        </aside>
      </div>

      <Dialog.Root
        bind:open={targetEditorOpen}
        onOpenChange={(open) => {
          if (open) {
            targetEditorClosing = false
          } else if (!targetEditorClosing && targetEditorTarget) {
            closeTargetEditor(false)
          }
        }}>
        {#if targetEditorTarget}
          {@const target = targetEditorTarget}
          {@const index = targetIndex(target)}
          {@const summary = selectedSummary(target)}
          <Dialog.Layout class="sm:max-w-4xl">
            {#snippet header()}
              <Dialog.Title id={`target-title-${target.key}`}>
                {m.model_editor_edit_destination_value({ index: index + 1 })}
              </Dialog.Title>
              <Dialog.Description>{m.model_editor_target_dialog_help()}</Dialog.Description>
            {/snippet}
            <div aria-labelledby={`target-title-${target.key}`}>
              <div class="mb-4 flex items-center justify-between gap-3 border-b pb-4">
                <div class="flex items-center gap-2">
                  <Badge variant={target.enabled ? 'default' : 'secondary'}>
                    {target.enabled ? m.model_editor_enabled() : m.model_editor_disabled()}
                  </Badge>
                  {#if target.persisted && summary && !summary.available}<Badge variant="destructive"
                      >{m.model_editor_model_no_longer_available()}</Badge
                    >{/if}
                  {#if providerOnlySearchUnavailable(target)}
                    <Badge variant="destructive">{m.model_editor_provider_only_search_unavailable()}</Badge>
                  {/if}
                </div>
                <div class="flex items-center gap-1">
                  {#if !target.enabled}
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      class="size-10"
                      aria-label={m.model_editor_remove_destination_value({ index: index + 1 })}
                      onclick={deleteEditedTarget}><Trash2Icon /></Button>
                  {/if}
                </div>
              </div>

              <Field.Group class="grid gap-4 lg:grid-cols-3">
                <Field.Field size="select">
                  <Field.Label for={`target-provider-${target.key}`}>{m.common_model_service()}</Field.Label>
                  <Select.Root
                    type="single"
                    value={target.providerId}
                    onValueChange={(value: string) => value && void changeProvider(target, value)}>
                    <Select.Trigger
                      id={`target-provider-${target.key}`}
                      class="w-full"
                      aria-label={m.model_editor_destination_value_model_service({ index: index + 1 })}>
                      {providers.find((provider) => provider.id === target.providerId)?.name ??
                        m.model_editor_choose_model_service()}
                    </Select.Trigger>
                    <Select.Content>
                      <Select.Group>
                        {#each availableProviders as provider (provider.id)}<Select.Item
                            value={provider.id}
                            label={provider.name}>{provider.name}</Select.Item
                          >{/each}
                      </Select.Group>
                    </Select.Content>
                  </Select.Root>
                </Field.Field>

                <Field.Field size="select">
                  <Field.Label for={`target-type-${target.key}`}>{m.model_editor_destination_type()}</Field.Label>
                  <Select.Root
                    type="single"
                    value={target.model === null ? 'provider_only' : 'model'}
                    onValueChange={(value: string) => value && void changeDestinationType(target, value)}>
                    <Select.Trigger id={`target-type-${target.key}`} class="w-full">
                      {target.model === null
                        ? m.model_editor_provider_only_search_destination()
                        : m.model_editor_model_destination()}
                    </Select.Trigger>
                    <Select.Content>
                      <Select.Group>
                        <Select.Item value="model">{m.model_editor_model_destination()}</Select.Item>
                        {#if target.model === null || providerOnlySearchSupported(target.providerId)}
                          <Select.Item value="provider_only">
                            {m.model_editor_provider_only_search_destination()}
                          </Select.Item>
                        {/if}
                      </Select.Group>
                    </Select.Content>
                  </Select.Root>
                </Field.Field>

                <Field.Field size="fill" data-invalid={providerOnlySearchUnavailable(target)}>
                  <Field.Label for={`target-model-${target.key}`}>{m.common_model()}</Field.Label>
                  {#if target.model === null}
                    <p
                      id={`target-model-${target.key}`}
                      class="min-h-10 rounded-lg border bg-muted/30 px-3 py-2 text-sm">
                      {m.model_editor_provider_only_search_destination()}
                    </p>
                    <Field.Description>{m.model_editor_provider_only_search_help()}</Field.Description>
                    {#if providerOnlySearchUnavailable(target)}
                      <p class="text-sm text-destructive" role="status">
                        {m.model_editor_provider_only_search_unavailable_help()}
                      </p>
                    {/if}
                  {:else if target.custom}
                    <Input
                      id={`target-model-${target.key}`}
                      class="font-technical"
                      bind:value={target.model}
                      aria-label={m.model_editor_destination_value_custom_model_id({ index: index + 1 })}
                      placeholder="private-model" />
                    <Field.Description class="text-warning"
                      >{m.model_editor_model_not_synced_list_requests_fail_if_id()}</Field.Description>
                    <Button class="mt-2" type="button" variant="ghost" size="sm" onclick={() => useInventory(target)}
                      >{m.model_editor_choose_synced_model()}</Button>
                  {:else}
                    <ModelCombobox
                      id={`target-model-${target.key}`}
                      value={target.model}
                      models={modelCandidates(target)}
                      placeholder={m.model_editor_choose_model()}
                      searchPlaceholder={m.model_editor_search_model_id()}
                      emptyText={m.model_editor_no_models_found()}
                      ariaLabel={m.model_editor_destination_value_model({ index: index + 1 })}
                      searchAriaLabel={m.model_editor_search_models_destination_value({ index: index + 1 })}
                      disabled={!target.providerId || target.loading}
                      onSelect={(value: string) => void selectModel(target, value)} />
                  {/if}
                </Field.Field>
              </Field.Group>

              <Field.Group class="mt-4 grid gap-4 border-t pt-4 md:grid-cols-3">
                <Field.Field size="number">
                  <Field.Label for={`target-first-token-timeout-${target.key}`}
                    >{m.model_editor_first_token_timeout()}</Field.Label>
                  <Input
                    id={`target-first-token-timeout-${target.key}`}
                    type="number"
                    min="0"
                    step="0.001"
                    bind:value={target.firstTokenTimeoutSeconds} />
                  <Field.Description>{m.model_editor_first_token_timeout_help()}</Field.Description>
                </Field.Field>
                <Field.Field size="number">
                  <Field.Label for={`target-retry-budget-${target.key}`}
                    >{m.model_editor_target_retry_budget()}</Field.Label>
                  <Input
                    id={`target-retry-budget-${target.key}`}
                    type="number"
                    min="0"
                    step="1"
                    bind:value={target.targetRetryBudget} />
                  <Field.Description>{m.model_editor_target_retry_budget_help()}</Field.Description>
                </Field.Field>
                <Field.Field size="number">
                  <Field.Label for={`target-cooldown-${target.key}`}>{m.model_editor_target_cooldown()}</Field.Label>
                  <Input
                    id={`target-cooldown-${target.key}`}
                    type="number"
                    min="0"
                    step="0.001"
                    bind:value={target.targetCooldownSeconds} />
                  <Field.Description>{m.model_editor_target_cooldown_help()}</Field.Description>
                </Field.Field>
              </Field.Group>

              {#if target.loading}
                <div class="mt-3 flex items-center gap-2 text-sm text-muted-foreground">
                  <Spinner />{m.model_editor_loading_models_supported_features()}
                </div>
              {:else if summary}
                <div class="mt-4 flex flex-wrap items-center gap-2 border-t pt-3">
                  <ModelSpecification specification={summary.specification} />
                  <ModelDetailsDialog
                    providerId={target.providerId}
                    modelId={target.model ?? ''}
                    triggerLabel={m.model_editor_view_model_details()} />
                </div>
              {/if}
              {#if target.providerId && target.model?.trim() && target.thinkingLevelMap.length > 0}
                <div class="mt-4 border-t pt-4">
                  <div class="mb-3 flex flex-wrap items-start justify-between gap-3">
                    <div>
                      <h4 class="font-medium">{m.model_editor_thinking_map()}</h4>
                      <p class="text-sm text-muted-foreground">{m.model_editor_thinking_map_help()}</p>
                    </div>
                    {#if target.id}
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        onclick={() => requestThinkingMapRegeneration(target)}>
                        {m.model_editor_thinking_regenerate()}
                      </Button>
                    {/if}
                  </div>
                  <div data-slot="thinking-map" class="divide-y border-y">
                    {#each target.thinkingLevelMap as row (row.level)}
                      {@const rowWritable = thinkingRowWritable(target, row)}
                      <div
                        data-slot="thinking-map-row"
                        class="grid grid-cols-[minmax(0,1fr)_2.5rem] items-center gap-x-3 gap-y-2 py-2 sm:grid-cols-[minmax(8rem,0.8fr)_minmax(0,2.2fr)_2.5rem]">
                        <div class="flex min-w-0 items-center gap-2">
                          <span class="font-technical text-sm">{row.level}</span>
                          {#if row.source === 'overridden'}
                            <span class="text-xs text-muted-foreground">
                              {m.model_editor_thinking_overridden()}
                            </span>
                          {/if}
                        </div>
                        <div class="col-span-2 flex min-w-0 gap-2 sm:col-span-1">
                          <Select.Root
                            type="single"
                            value={row.control.type}
                            onValueChange={(value: string) =>
                              value && changeThinkingControlKind(row, value as TargetThinkingControl['type'])}>
                            <Select.Trigger
                              class={['w-28 shrink-0 sm:w-32', !rowWritable && 'border-destructive']}
                              aria-invalid={!rowWritable}
                              aria-label={`${row.level} ${m.model_editor_thinking_control()}`}>
                              {thinkingControlLabel(row.control.type)}
                            </Select.Trigger>
                            <Select.Content>
                              <Select.Group>
                                {#each thinkingControlOptions(target, row) as type (type)}
                                  <Select.Item value={type}>{thinkingControlLabel(type)}</Select.Item>
                                {/each}
                              </Select.Group>
                            </Select.Content>
                          </Select.Root>
                          {#if row.control.type === 'effort'}
                            <Input
                              class="min-w-0 flex-1"
                              value={row.control.value}
                              aria-label={`${row.level} ${m.model_editor_thinking_effort()}`}
                              oninput={(event: Event) => changeThinkingControlValue(row, inputValue(event))} />
                          {:else if row.control.type === 'budget'}
                            <Input
                              class="min-w-0 flex-1"
                              type="number"
                              min="0"
                              step="1"
                              value={row.control.value}
                              aria-label={`${row.level} ${m.model_editor_thinking_budget()}`}
                              oninput={(event: Event) => changeThinkingControlValue(row, inputValue(event))} />
                          {/if}
                        </div>
                        {#if target.id}
                          <Button
                            type="button"
                            variant="ghost"
                            size="icon"
                            class="col-start-2 row-start-1 size-10 justify-self-end sm:col-auto sm:row-auto"
                            aria-label={`${m.model_editor_thinking_reset_row()}: ${row.level}`}
                            title={m.model_editor_thinking_reset_row()}
                            onclick={() => void resetThinkingRow(target, row.level)}>
                            <RotateCcwIcon />
                          </Button>
                        {/if}
                        {#if !rowWritable}
                          <p class="col-span-full text-sm text-destructive" role="status">
                            {thinkingRowHint(target, row)}
                          </p>
                        {/if}
                      </div>
                    {/each}
                  </div>
                </div>
              {/if}
              {#if target.validationError}<p class="mt-3 text-sm text-warning" role="status">
                  {target.validationError}
                </p>{/if}
            </div>
            {#snippet footer()}
              <Button type="button" variant="outline" onclick={() => closeTargetEditor(false)}>
                {m.common_cancel()}
              </Button>
              <Button
                type="button"
                disabled={target.enabled && !targetConfigured(target)}
                onclick={() => closeTargetEditor(true)}>{m.common_confirm()}</Button>
            {/snippet}
          </Dialog.Layout>
        {/if}
      </Dialog.Root>
    </section>

    <section class="route-section" aria-labelledby="route-thinking-title">
      <div class="route-section-header">
        <div>
          <h2 id="route-thinking-title" class="route-section-title">{m.model_editor_thinking_levels()}</h2>
          <p class="route-section-description">{m.model_editor_thinking_levels_help()}</p>
        </div>
      </div>
      <div class="grid grid-cols-2 gap-1 rounded-xl bg-muted/60 p-1 sm:grid-cols-4 xl:grid-cols-7">
        {#each thinkingLevels as level (level)}
          {@const blockers = thinkingLevelBlockers(level)}
          {#if blockers.length > 0}
            <Tooltip.Root>
              <Tooltip.Trigger
                type="button"
                data-slot="route-thinking-level"
                data-level={level}
                data-supported="false"
                class="flex min-h-10 w-full cursor-help items-center justify-center rounded-lg px-3 text-muted-foreground outline-none transition-colors hover:bg-background/50 focus-visible:ring-[3px] focus-visible:ring-ring/50">
                <span class="font-technical">{level}</span>
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
            <div
              data-slot="route-thinking-level"
              data-level={level}
              data-supported="true"
              class="flex min-h-10 items-center justify-center rounded-lg bg-background px-3 shadow-xs">
              <span class="font-technical">{level}</span>
            </div>
          {/if}
        {/each}
      </div>
      <Field.Field orientation="vertical" class="mt-4 sm:max-w-md">
        <Field.Label for="route-default-thinking-level">{m.model_editor_default_thinking_level()}</Field.Label>
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
        <Field.Description>{m.model_editor_default_thinking_level_help()}</Field.Description>
        {#if defaultThinkingLevelUnsupported}
          <p data-slot="default-thinking-level-warning" class="text-sm text-destructive">
            {m.model_editor_default_thinking_level_unavailable({ level: form.defaultThinkingLevel })}
          </p>
        {/if}
      </Field.Field>
    </section>

    <div
      data-slot="model-editor-footer"
      class="sticky bottom-0 z-20 mt-auto flex translate-y-2 flex-wrap items-center justify-between gap-3 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
      <p class="text-sm text-muted-foreground">
        {targets.length === 1
          ? m.common_1_destination()
          : m.model_editor_value_destinations({ target_count: targets.length })} · {strategySummary(form.balance)}
      </p>
      <div class="flex gap-2">
        <Button href="/models" variant="outline">{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving}
          >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_model()}</Button>
      </div>
    </div>
  </form>
</div>

<AlertDialog.Root bind:open={regenerateOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.model_editor_thinking_regenerate()}</AlertDialog.Title>
      <AlertDialog.Description>{m.model_editor_thinking_regenerate_confirm()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel onclick={() => (regenerateTarget = undefined)}>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action onclick={() => void regenerateThinkingMap()}>
        {m.model_editor_thinking_regenerate()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

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
