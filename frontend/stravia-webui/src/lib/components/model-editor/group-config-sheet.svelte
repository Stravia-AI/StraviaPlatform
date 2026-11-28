<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw'
import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert'
import { formatList } from '#lib/format.js'
import { inputValue } from '#lib/utils.js'
import {
  defaultEffortControl,
  writableThinkingControlKinds,
  type ThinkingControlContext,
} from '#lib/thinking-control.js'
import type {
  ModelSpecification as ModelSpecificationData,
  TargetThinkingControl,
  ThinkingLevel,
  ThinkingLevelMapping,
} from '#lib/types.js'
import {
  checkGroupThinkingCompatibility,
  commonGroupEffortSupport,
  isValidTargetRetryBudget,
  isValidTargetSeconds,
  type RouteTargetForm,
  type RouteTargetGroupScope,
  type TargetGroupConfig,
} from '../route-targets-form.js'
import ModelSpecification from '#lib/components/model-specification.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Checkbox } from '#lib/components/ui/checkbox/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

interface Props {
  open: boolean
  sessionId: number
  model: string
  modelDisplayName: string
  scope: RouteTargetGroupScope
  layerNumber: number | null
  members: RouteTargetForm[]
  initialConfig: TargetGroupConfig
  specification?: ModelSpecificationData
  savedRouteId: string
  providerLabel: (target: RouteTargetForm) => string
  thinkingContextFor: (target: RouteTargetForm) => ThinkingControlContext
  onClose: () => void
  onApply: (nextConfig: TargetGroupConfig, overwriteCustom: boolean) => void
  onInspectMember: (target: RouteTargetForm) => void
  onRegenerateGroup: (overwriteCustom: boolean) => Promise<ThinkingLevelMapping[] | null>
  onResetGroupRow: (level: ThinkingLevel, overwriteCustom: boolean) => Promise<ThinkingLevelMapping[] | null>
}

const thinkingControlKinds = ['effort', 'hidden'] as const

let {
  open,
  sessionId,
  model,
  modelDisplayName,
  scope,
  layerNumber,
  members,
  initialConfig,
  specification,
  savedRouteId,
  providerLabel,
  thinkingContextFor,
  onClose,
  onApply,
  onInspectMember,
  onRegenerateGroup,
  onResetGroupRow,
}: Props = $props()

let firstTokenTimeoutSeconds = $state(120)
let targetRetryBudget = $state(5)
let targetCooldownSeconds = $state(120)
let thinkingLevelMap = $state<ThinkingLevelMapping[]>([])
let overwriteCustom = $state(false)
let busyThinkingAction = $state(false)
let actionError = $state('')
let lastSessionId = -1

$effect(() => {
  if (open && sessionId !== lastSessionId) {
    lastSessionId = sessionId
    firstTokenTimeoutSeconds = initialConfig.firstTokenTimeoutSeconds
    targetRetryBudget = initialConfig.targetRetryBudget
    targetCooldownSeconds = initialConfig.targetCooldownSeconds
    thinkingLevelMap = initialConfig.thinkingLevelMap.map((row) => ({ ...row, control: { ...row.control } }))
    overwriteCustom = false
    actionError = ''
  }
})

const sharedMembers = $derived(members.filter((member) => member.configMode === 'shared'))
const customMembers = $derived(members.filter((member) => member.configMode === 'custom'))
const affectedMembers = $derived(overwriteCustom ? members : sharedMembers)
const compatBasis = $derived(affectedMembers.length > 0 ? affectedMembers : members)
const membersLoading = $derived(members.some((member) => member.loading))

const memberContexts = $derived(
  compatBasis.map((member) => ({
    label: providerLabel(member),
    context: thinkingContextFor(member),
  })),
)

const effortSupport = $derived(commonGroupEffortSupport(memberContexts))
const compatIssues = $derived(checkGroupThinkingCompatibility(memberContexts, thinkingLevelMap))
const compatIssueByLevel = $derived(new Map(compatIssues.map((issue) => [issue.level, issue] as const)))

const effortKindAllowed = $derived.by(() => {
  if (effortSupport.common !== null) return effortSupport.common.length > 0
  return memberContexts.every((entry) => writableThinkingControlKinds(entry.context).includes('effort'))
})

const firstTokenInvalid = $derived(!isValidTargetSeconds(firstTokenTimeoutSeconds))
const retryBudgetInvalid = $derived(!isValidTargetRetryBudget(targetRetryBudget))
const cooldownInvalid = $derived(!isValidTargetSeconds(targetCooldownSeconds))
const hasEmptyEffort = $derived(
  thinkingLevelMap.some((row) => row.control.type === 'effort' && !row.control.value.trim()),
)

const canCallServerGroupThinking = $derived.by(() => {
  if (!savedRouteId) return false
  const persistedAffected = affectedMembers.filter((member) => member.persisted && Boolean(member.id))
  if (persistedAffected.length === 0) return false
  if (scope.enabled) return true
  const firstPriority = persistedAffected[0].priority
  return persistedAffected.every((member) => member.priority === firstPriority)
})

const canApply = $derived(
  !busyThinkingAction &&
    !membersLoading &&
    affectedMembers.length > 0 &&
    !firstTokenInvalid &&
    !retryBudgetInvalid &&
    !cooldownInvalid &&
    compatIssues.length === 0 &&
    !hasEmptyEffort,
)

const showTechnicalModelId = $derived(Boolean(model.trim() && modelDisplayName !== model.trim()))

function scopeLabel(): string {
  return scope.enabled && layerNumber !== null
    ? m.model_editor_layer_value({ index: layerNumber })
    : m.model_editor_disabled()
}

function thinkingControlLabel(type: TargetThinkingControl['type']): string {
  return type === 'effort' ? m.model_editor_thinking_effort() : m.model_editor_thinking_hidden()
}

function changeThinkingControlKind(row: ThinkingLevelMapping, type: TargetThinkingControl['type']): void {
  if (type === 'effort') {
    const syntheticContext: ThinkingControlContext = {
      protocol: memberContexts[0]?.context.protocol,
      reasoningEfforts: effortSupport.common,
    }
    row.control = defaultEffortControl(row.level, syntheticContext)
  } else {
    row.control = { type: 'hidden' }
  }
  row.source = 'overridden'
  actionError = ''
}

function changeThinkingEffort(row: ThinkingLevelMapping, value: string): void {
  row.control = { type: 'effort', value }
  row.source = 'overridden'
  actionError = ''
}

async function handleRegenerateGroup(): Promise<void> {
  if (!canCallServerGroupThinking || busyThinkingAction) return
  busyThinkingAction = true
  actionError = ''
  try {
    const nextMap = await onRegenerateGroup(overwriteCustom)
    if (nextMap) {
      thinkingLevelMap = nextMap.map((row) => ({ ...row, control: { ...row.control } }))
    }
  } catch (error) {
    actionError = error instanceof Error ? error.message : String(error)
  } finally {
    busyThinkingAction = false
  }
}

async function handleResetGroupRow(level: ThinkingLevel): Promise<void> {
  if (!canCallServerGroupThinking || busyThinkingAction) return
  busyThinkingAction = true
  actionError = ''
  try {
    const nextMap = await onResetGroupRow(level, overwriteCustom)
    if (nextMap) {
      thinkingLevelMap = nextMap.map((row) => ({ ...row, control: { ...row.control } }))
    }
  } catch (error) {
    actionError = error instanceof Error ? error.message : String(error)
  } finally {
    busyThinkingAction = false
  }
}

function handleApply(): void {
  if (!canApply) return
  onApply(
    {
      firstTokenTimeoutSeconds: Number(firstTokenTimeoutSeconds),
      targetRetryBudget: Number(targetRetryBudget),
      targetCooldownSeconds: Number(targetCooldownSeconds),
      thinkingLevelMap: thinkingLevelMap.map((row) => ({ ...row, control: { ...row.control } })),
    },
    overwriteCustom,
  )
}
</script>

<Sheet.Root
  {open}
  onOpenChange={(nextOpen) => {
    if (!nextOpen) onClose()
  }}>
  <Sheet.Content
    side="right"
    data-slot="group-config-sheet"
    class="w-full! gap-0 overflow-hidden p-0 sm:max-w-xl!"
    locked={busyThinkingAction}
    closeLabel={m.common_close()}>
    <Sheet.Header class="border-b pr-12">
      <div class="flex flex-wrap items-center gap-2">
        <Sheet.Title class="wrap-anywhere">{m.model_editor_group_config_title({ model: modelDisplayName })}</Sheet.Title>
      </div>
      {#if showTechnicalModelId}
        <p class="truncate font-technical text-xs text-muted-foreground">{model}</p>
      {/if}
      <div class="mt-1.5 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground tabular-nums">
        <Badge variant={scope.enabled && layerNumber === 1 ? 'default' : 'secondary'}>{scopeLabel()}</Badge>
        <span>
          {members.length === 1
            ? m.model_editor_group_single_account()
            : m.model_editor_group_accounts_count({ count: members.length })}
        </span>
        <span aria-hidden="true">·</span>
        <span>{m.model_editor_group_shared_count({ count: sharedMembers.length })}</span>
        {#if customMembers.length > 0}
          <span aria-hidden="true">·</span>
          <span class="font-medium text-warning">{m.model_editor_group_custom_count({ count: customMembers.length })}</span>
        {/if}
      </div>
    </Sheet.Header>

    <div class="route-overlay-body flex flex-col gap-5">
      <div class="flex flex-col gap-2">
        <div class="flex flex-wrap items-baseline justify-between gap-2">
          <h3 class="text-sm font-medium text-foreground">{m.model_editor_group_members()}</h3>
          <span class="text-xs text-muted-foreground">{m.model_editor_group_members_hint()}</span>
        </div>
        <div class="flex flex-wrap gap-1.5">
          {#each members as member (member.key)}
            {@const memberIsCustom = member.configMode === 'custom'}
            <button
              type="button"
              class={[
                'inline-flex max-w-full items-center gap-1.5 rounded-md border px-2 py-1 text-xs transition-colors hover:border-primary/50 focus-visible:outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50',
                memberIsCustom
                  ? 'border-warning/50 bg-warning/[0.06] text-foreground'
                  : 'border-border bg-card text-foreground',
              ]}
              onclick={() => onInspectMember(member)}>
              <span class="max-w-[14rem] truncate font-medium">{providerLabel(member)}</span>
              {#if memberIsCustom}
                <span class="rounded bg-warning/15 px-1 py-0.5 text-[11px] font-medium leading-none text-warning">
                  {m.model_editor_custom_badge()}
                </span>
              {/if}
            </button>
          {/each}
        </div>
      </div>

      {#if specification}
        <div class="border-t pt-4">
          <ModelSpecification {specification} />
        </div>
      {/if}

      {#if customMembers.length > 0}
        <div class="flex flex-col gap-2 rounded-lg border border-warning/40 bg-warning/[0.05] p-3">
          <label for="group-overwrite-custom" class="flex cursor-pointer items-start gap-2.5 text-sm">
            <Checkbox id="group-overwrite-custom" class="mt-0.5" bind:checked={overwriteCustom} />
            <span class="leading-snug text-foreground">
              {m.model_editor_group_overwrite_custom({ count: customMembers.length })}
            </span>
          </label>
          {#if sharedMembers.length === 0 && !overwriteCustom}
            <p class="pl-6 text-xs text-warning" role="status">{m.model_editor_group_all_custom_hint()}</p>
          {/if}
        </div>
      {/if}

      <Field.Group class="grid gap-4 border-t pt-4 sm:grid-cols-3">
        <Field.Field orientation="vertical" data-invalid={firstTokenInvalid ? 'true' : undefined}>
          <Field.Label for="group-first-token-timeout" hint={m.model_editor_first_token_timeout_help()}>
            {m.model_editor_first_token_timeout()}
          </Field.Label>
          <InputGroup.Root class="w-28">
            <InputGroup.Input
              id="group-first-token-timeout"
              type="number"
              min="0"
              step="0.001"
              aria-invalid={firstTokenInvalid}
              aria-describedby={firstTokenInvalid ? 'group-first-token-timeout-constraint' : undefined}
              bind:value={firstTokenTimeoutSeconds} />
            <InputGroup.Addon align="inline-end">
              <InputGroup.Text>{m.model_editor_unit_seconds()}</InputGroup.Text>
            </InputGroup.Addon>
          </InputGroup.Root>
          {#if firstTokenInvalid}
            <Field.Error id="group-first-token-timeout-constraint">{m.model_editor_seconds_constraint()}</Field.Error>
          {/if}
        </Field.Field>

        <Field.Field orientation="vertical" data-invalid={retryBudgetInvalid ? 'true' : undefined}>
          <Field.Label for="group-retry-budget" hint={m.model_editor_target_retry_budget_help()}>
            {m.model_editor_target_retry_budget()}
          </Field.Label>
          <InputGroup.Root class="w-28">
            <InputGroup.Input
              id="group-retry-budget"
              type="number"
              min="0"
              step="1"
              aria-invalid={retryBudgetInvalid}
              aria-describedby={retryBudgetInvalid ? 'group-retry-budget-constraint' : undefined}
              bind:value={targetRetryBudget} />
            <InputGroup.Addon align="inline-end">
              <InputGroup.Text>{m.model_editor_unit_times()}</InputGroup.Text>
            </InputGroup.Addon>
          </InputGroup.Root>
          {#if retryBudgetInvalid}
            <Field.Error id="group-retry-budget-constraint">{m.model_editor_retry_budget_constraint()}</Field.Error>
          {/if}
        </Field.Field>

        <Field.Field orientation="vertical" data-invalid={cooldownInvalid ? 'true' : undefined}>
          <Field.Label for="group-cooldown" hint={m.model_editor_target_cooldown_help()}>
            {m.model_editor_target_cooldown()}
          </Field.Label>
          <InputGroup.Root class="w-28">
            <InputGroup.Input
              id="group-cooldown"
              type="number"
              min="0"
              step="0.001"
              aria-invalid={cooldownInvalid}
              aria-describedby={cooldownInvalid ? 'group-cooldown-constraint' : undefined}
              bind:value={targetCooldownSeconds} />
            <InputGroup.Addon align="inline-end">
              <InputGroup.Text>{m.model_editor_unit_seconds()}</InputGroup.Text>
            </InputGroup.Addon>
          </InputGroup.Root>
          {#if cooldownInvalid}
            <Field.Error id="group-cooldown-constraint">{m.model_editor_seconds_constraint()}</Field.Error>
          {/if}
        </Field.Field>
      </Field.Group>

      {#if membersLoading}
        <div class="flex items-center gap-2 border-t pt-4 text-sm text-muted-foreground">
          <Spinner />{m.model_editor_loading_models_supported_features()}
        </div>
      {:else if thinkingLevelMap.length > 0}
        <div class="flex flex-col gap-3 border-t pt-4">
          <div class="flex flex-wrap items-start justify-between gap-3">
            <div class="min-w-0">
              <div class="flex items-center gap-2">
                <h4 class="font-medium">{m.model_editor_thinking_map()}</h4>
                <Field.Help hint={m.model_editor_thinking_map_help()} />
              </div>
              {#if canCallServerGroupThinking}
                <p class="mt-1 text-xs text-muted-foreground">{m.model_editor_thinking_map_immediate()}</p>
              {/if}
            </div>
            {#if canCallServerGroupThinking}
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={busyThinkingAction}
                onclick={() => void handleRegenerateGroup()}>
                {#if busyThinkingAction}<Spinner data-icon="inline-start" />{/if}
                {m.model_editor_group_regenerate_thinking()}
              </Button>
            {/if}
          </div>

          {#if effortSupport.differences.length > 0}
            <Alert.Root variant="warning">
              <TriangleAlertIcon />
              <Alert.Title>{m.model_editor_group_thinking_diff_warning()}</Alert.Title>
              <Alert.Description>
                <ul class="mt-1 flex list-disc flex-col gap-0.5 pl-4 text-xs">
                  {#each effortSupport.differences as diff (diff.label)}
                    <li>
                      {m.model_editor_group_thinking_member_missing({
                        provider: diff.label,
                        efforts: formatList(diff.missing),
                      })}
                    </li>
                  {/each}
                </ul>
              </Alert.Description>
            </Alert.Root>
          {/if}

          {#if effortSupport.common !== null && effortSupport.common.length === 0}
            <Alert.Root variant="warning">
              <TriangleAlertIcon />
              <Alert.Description>{m.model_editor_group_thinking_no_common_efforts()}</Alert.Description>
            </Alert.Root>
          {/if}

          <div data-slot="thinking-map" class="divide-y border-y">
            {#each thinkingLevelMap as row (row.level)}
              {@const issue = compatIssueByLevel.get(row.level)}
              {@const emptyEffort = row.control.type === 'effort' && !row.control.value.trim()}
              {@const rowInvalid = Boolean(issue) || emptyEffort}
              {@const errorId = `group-thinking-error-${row.level}`}
              <div
                data-slot="thinking-map-row"
                class="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-2 py-2 sm:grid-cols-[minmax(7.5rem,0.8fr)_minmax(0,2.2fr)_auto]">
                <div class="flex min-w-0 items-center gap-2">
                  <span class="font-technical text-sm">{row.level}</span>
                  {#if row.source === 'overridden'}
                    <span class="text-xs text-muted-foreground">{m.model_editor_thinking_overridden()}</span>
                  {/if}
                </div>
                <div class="col-span-2 flex min-w-0 gap-2 sm:col-span-1">
                  <Select.Root
                    type="single"
                    value={row.control.type}
                    onValueChange={(value: string) =>
                      value && changeThinkingControlKind(row, value as TargetThinkingControl['type'])}>
                    <Select.Trigger
                      class="w-36 shrink-0"
                      aria-invalid={row.control.type === 'effort' && !effortKindAllowed}
                      aria-describedby={rowInvalid ? errorId : undefined}
                      aria-label={m.model_editor_thinking_control({ level: row.level })}>
                      {thinkingControlLabel(row.control.type)}
                    </Select.Trigger>
                    <Select.Content>
                      <Select.Group>
                        {#each thinkingControlKinds as type (type)}
                          <Select.Item value={type} disabled={type === 'effort' && !effortKindAllowed}>
                            {thinkingControlLabel(type)}
                          </Select.Item>
                        {/each}
                      </Select.Group>
                    </Select.Content>
                  </Select.Root>

                  {#if row.control.type === 'effort'}
                    {#if effortSupport.common && effortSupport.common.length > 0}
                      {@const current = row.control.value}
                      <Select.Root
                        type="single"
                        value={current}
                        onValueChange={(value: string) => value && changeThinkingEffort(row, value)}>
                        <Select.Trigger
                          title={current}
                          class="min-w-0 flex-1 font-technical"
                          aria-invalid={rowInvalid}
                          aria-describedby={rowInvalid ? errorId : undefined}
                          aria-label={m.model_editor_thinking_upstream_level({ level: row.level })}>
                          <span class={['truncate', !current.trim() && 'font-sans text-muted-foreground']}>
                            {current.trim() ? current : m.model_editor_thinking_choose_level()}
                          </span>
                        </Select.Trigger>
                        <Select.Content class="max-w-[calc(100vw-2rem)]">
                          <Select.Group>
                            {#each effortSupport.common as effort (effort)}
                              <Select.Item value={effort} class="font-technical">
                                <span class="min-w-0 whitespace-normal wrap-anywhere">{effort}</span>
                              </Select.Item>
                            {/each}
                          </Select.Group>
                        </Select.Content>
                      </Select.Root>
                    {:else}
                      <Input
                        class="min-w-0 flex-1 font-technical"
                        value={row.control.value}
                        aria-label={m.model_editor_thinking_upstream_level({ level: row.level })}
                        readonly={effortSupport.common !== null}
                        aria-invalid={rowInvalid}
                        aria-describedby={rowInvalid ? errorId : undefined}
                        oninput={(event: Event) => changeThinkingEffort(row, inputValue(event))} />
                    {/if}
                  {/if}
                </div>

                {#if canCallServerGroupThinking}
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    class="col-start-2 row-start-1 justify-self-end sm:col-auto sm:row-auto"
                    disabled={busyThinkingAction}
                    aria-label={`${m.model_editor_thinking_reset_row()}: ${row.level}`}
                    title={m.model_editor_thinking_reset_row()}
                    onclick={() => void handleResetGroupRow(row.level)}>
                    <RotateCcwIcon />
                  </Button>
                {/if}

                {#if issue}
                  <p id={errorId} class="col-span-full text-sm wrap-anywhere text-destructive" role="status">
                    {m.model_editor_group_thinking_incompatible({ members: formatList(issue.incompatibleMembers) })}
                  </p>
                {:else if emptyEffort}
                  <p id={errorId} class="col-span-full text-sm wrap-anywhere text-destructive" role="status">
                    {m.model_editor_thinking_effort_required()}
                  </p>
                {/if}
              </div>
            {/each}
          </div>
        </div>
      {/if}

      {#if actionError}
        <p class="text-sm text-destructive" role="alert">{actionError}</p>
      {/if}
    </div>

    <div class="route-overlay-footer">
      <p class="mr-auto text-xs text-muted-foreground tabular-nums">
        {m.model_editor_group_apply_hint({ affected: affectedMembers.length, total: members.length })}
      </p>
      <Button type="button" variant="outline" disabled={busyThinkingAction} onclick={onClose}>
        {m.common_cancel()}
      </Button>
      <Button type="button" disabled={!canApply} onclick={handleApply}>
        {m.model_editor_group_apply()}
      </Button>
    </div>
  </Sheet.Content>
</Sheet.Root>
