<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import LockIcon from '@lucide/svelte/icons/lock'
import RotateCcwIcon from '@lucide/svelte/icons/rotate-ccw'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import Trash2Icon from '@lucide/svelte/icons/trash-2'
import { formatList } from '#lib/format.js'
import { routePurposeLabels } from '#lib/logical-model.js'
import { idNameOption } from '#lib/search-combobox-option.js'
import {
  defaultEffortControl,
  effortCandidates,
  thinkingControlWritable,
  unrepresentableThinkingLevels,
  writableThinkingControlKinds,
  type ThinkingControlContext,
} from '#lib/thinking-control.js'
import type {
  Provider,
  ProviderModelSummary,
  RoutePurpose,
  TargetConfigMode,
  TargetThinkingControl,
  ThinkingLevel,
  ThinkingLevelMapping,
} from '#lib/types.js'
import { inputValue } from '#lib/utils.js'
import {
  applyTargetGroupConfig,
  isValidTargetRetryBudget,
  isValidTargetSeconds,
  summarizeTargetGroupConfig,
  type RouteTargetForm,
  type TargetGroupConfig,
} from '../route-targets-form.js'
import ModelDetailsDialog from '#lib/components/model-details-dialog.svelte'
import ModelSpecification from '#lib/components/model-specification.svelte'
import SearchCombobox from '#lib/components/search-combobox.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import { SegmentedControl, type SegmentedControlOption } from '#lib/components/ui/segmented-control/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

interface Props {
  open: boolean
  target: RouteTargetForm | undefined
  index: number
  isNew: boolean
  wasInitiallyEnabled: boolean
  purpose: RoutePurpose | null
  providers: Provider[]
  otherEnabledTargetCount: number
  /** Total members in the edited target's current (scope, model) group (including this target). */
  groupMemberCount: number
  /** Shared config of a peer in the same (scope, model) group, if one exists. */
  sharedPeerConfig: TargetGroupConfig | undefined
  providerLabel: (target: RouteTargetForm) => string
  modelDisplayName: (target: RouteTargetForm) => string
  selectedSummary: (target: RouteTargetForm) => ProviderModelSummary | undefined
  modelCandidates: (target: RouteTargetForm) => ProviderModelSummary[]
  thinkingContextFor: (target: RouteTargetForm, summary: ProviderModelSummary | undefined) => ThinkingControlContext
  onOpenChange: (open: boolean) => void
  onClose: (save: boolean) => void
  onDelete: () => void
  onChangeProvider: (target: RouteTargetForm, providerId: string) => Promise<void>
  onSelectModel: (target: RouteTargetForm, modelId: string) => Promise<void>
  onUseInventory: (target: RouteTargetForm) => void
  onResetThinkingRow: (target: RouteTargetForm, level: ThinkingLevel) => Promise<void>
  onRegenerateThinkingMap: (target: RouteTargetForm) => Promise<void>
  onOpenGroupConfig: () => void
}

const thinkingControlKinds = ['effort', 'hidden'] as const

let {
  open,
  target,
  index,
  isNew,
  wasInitiallyEnabled,
  purpose,
  providers,
  otherEnabledTargetCount,
  groupMemberCount,
  sharedPeerConfig,
  providerLabel,
  modelDisplayName,
  selectedSummary,
  modelCandidates,
  thinkingContextFor,
  onOpenChange,
  onClose,
  onDelete,
  onChangeProvider,
  onSelectModel,
  onUseInventory,
  onResetThinkingRow,
  onRegenerateThinkingMap,
  onOpenGroupConfig,
}: Props = $props()

const configModeOptions = $derived<readonly SegmentedControlOption<TargetConfigMode>[]>([
  { value: 'shared', label: m.model_editor_config_mode_shared() },
  { value: 'custom', label: m.model_editor_config_mode_custom() },
])

function targetConfigured(candidate: RouteTargetForm): boolean {
  return Boolean(candidate.providerId && candidate.model.trim())
}

function thinkingControlLabel(type: TargetThinkingControl['type']): string {
  return type === 'effort' ? m.model_editor_thinking_effort() : m.model_editor_thinking_hidden()
}

function formatThinkingLevelsSummary(levels: readonly ThinkingLevel[]): string {
  if (levels.length === 0) return m.model_editor_thinking_none_enabled()
  if (levels.length === 1) return levels[0]
  return `${levels[0]}–${levels[levels.length - 1]}`
}

function changeThinkingControlKind(
  edited: RouteTargetForm,
  row: ThinkingLevelMapping,
  type: TargetThinkingControl['type'],
  context: ThinkingControlContext,
): void {
  row.control = type === 'effort' ? defaultEffortControl(row.level, context) : { type }
  row.source = 'overridden'
  edited.validationError = ''
}

function changeThinkingEffort(edited: RouteTargetForm, row: ThinkingLevelMapping, value: string): void {
  row.control = { type: 'effort', value }
  row.source = 'overridden'
  edited.validationError = ''
}

function handleConfigModeChange(edited: RouteTargetForm, mode: TargetConfigMode): void {
  if (edited.configMode === mode) return
  edited.configMode = mode
  if (mode === 'shared' && sharedPeerConfig) {
    applyTargetGroupConfig(edited, sharedPeerConfig)
  }
  edited.validationError = ''
}

function handleToggleEnabled(edited: RouteTargetForm, checked: boolean, summary: ProviderModelSummary | undefined): void {
  if (!checked) {
    if (wasInitiallyEnabled && otherEnabledTargetCount === 0 && edited.enabled) return
    edited.enabled = false
    edited.validationError = ''
    return
  }
  if (!targetConfigured(edited)) {
    edited.validationError = m.model_editor_complete_target_before_enabling()
    return
  }
  const unwritable = unrepresentableThinkingLevels(edited.thinkingLevelMap, thinkingContextFor(edited, summary))
  if (unwritable.length > 0) {
    edited.validationError = m.model_editor_thinking_enable_blocked({ levels: formatList(unwritable) })
    return
  }
  edited.enabled = true
  edited.validationError = ''
}
</script>

<Sheet.Root {open} {onOpenChange}>
  {#if target}
    {@const summary = selectedSummary(target)}
    {@const configured = targetConfigured(target)}
    {@const firstTokenInvalid = !isValidTargetSeconds(target.firstTokenTimeoutSeconds)}
    {@const retryBudgetInvalid = !isValidTargetRetryBudget(target.targetRetryBudget)}
    {@const cooldownInvalid = !isValidTargetSeconds(target.targetCooldownSeconds)}
    {@const hasGroupPeers = groupMemberCount >= 2}
    {@const showConfigModePanel = target.model.trim().length > 0 && (hasGroupPeers || target.configMode === 'custom')}
    {@const isInheritedReadOnly = hasGroupPeers && target.configMode === 'shared'}
    {@const isLastEnabled = target.enabled && wasInitiallyEnabled && otherEnabledTargetCount === 0}
    {@const effectiveConfig = sharedPeerConfig ?? {
      firstTokenTimeoutSeconds: target.firstTokenTimeoutSeconds,
      targetRetryBudget: target.targetRetryBudget,
      targetCooldownSeconds: target.targetCooldownSeconds,
      thinkingLevelMap: target.thinkingLevelMap,
    }}
    {@const inheritedSummary = summarizeTargetGroupConfig(effectiveConfig)}
    {@const currentModelName = modelDisplayName(target)}
    {@const currentProviderName = providerLabel(target)}

    <Sheet.Content
      side="right"
      data-slot="target-detail-sheet"
      class="w-full! gap-0 overflow-hidden p-0 sm:max-w-xl!"
      closeLabel={m.common_close()}>
      <Sheet.Header class="border-b pr-12">
        <div class="flex flex-wrap items-center gap-2">
          <Sheet.Title id={`target-title-${target.key}`}>
            {m.model_editor_edit_destination_value({ index: index + 1 })}
          </Sheet.Title>
          <Badge variant={target.enabled ? 'default' : 'secondary'}>
            {target.enabled ? m.model_editor_enabled() : m.model_editor_disabled()}
          </Badge>
          {#if target.configMode === 'custom'}
            <Badge variant="outline" class="border-warning/50 bg-warning/10 text-warning">
              {m.model_editor_custom_badge()}
            </Badge>
          {/if}
          {#if target.persisted && summary && !summary.available}
            <Badge variant="destructive">{m.model_editor_model_no_longer_available()}</Badge>
          {/if}
        </div>
      </Sheet.Header>

      <div class="route-overlay-body flex flex-col gap-5" aria-labelledby={`target-title-${target.key}`}>
        <div class="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-border bg-muted/25 px-3 py-2.5">
          <div class="min-w-0 flex-1">
            <label for={`target-enabled-${target.key}`} class="block text-sm font-medium text-foreground">
              {m.model_editor_target_enable_toggle()}
            </label>
            {#if isLastEnabled}
              <p class="mt-0.5 text-xs text-muted-foreground">{m.model_editor_last_enabled_target_required()}</p>
            {:else if !target.enabled && !configured}
              <p class="mt-0.5 text-xs text-muted-foreground">{m.model_editor_complete_target_before_enabling()}</p>
            {/if}
          </div>
          <div class="flex shrink-0 items-center gap-2">
            <Switch
              id={`target-enabled-${target.key}`}
              aria-label={m.model_editor_target_enable_toggle()}
              disabled={isLastEnabled || (!target.enabled && !configured)}
              bind:checked={
                () => target.enabled, (checked: boolean) => handleToggleEnabled(target, checked, summary)
              } />
            {#if !target.enabled}
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label={m.model_editor_remove_destination_value({ index: index + 1 })}
                onclick={onDelete}>
                <Trash2Icon />
              </Button>
            {/if}
          </div>
        </div>

        <Field.Group class="grid gap-4 sm:grid-cols-2">
          <Field.Field>
            <Field.Label for={`target-provider-${target.key}`}>{m.common_model_service()}</Field.Label>
            <SearchCombobox
              id={`target-provider-${target.key}`}
              value={target.providerId}
              options={providers.map((provider) => ({ value: provider.id, label: provider.name }))}
              placeholder={m.model_editor_choose_model_service()}
              emptyText={m.common_no_matching_providers()}
              ariaLabel={m.model_editor_destination_value_model_service({ index: index + 1 })}
              onValueChange={(value: string) => void onChangeProvider(target, value)} />
          </Field.Field>

          <Field.Field>
            <Field.Label for={`target-model-${target.key}`}>{m.common_model()}</Field.Label>
            {#if target.custom}
              <Input
                id={`target-model-${target.key}`}
                class="font-technical"
                bind:value={target.model}
                aria-label={m.model_editor_destination_value_custom_model_id({ index: index + 1 })}
                placeholder="private-model" />
              <Field.Description class="text-warning">
                {m.model_editor_model_not_synced_list_requests_fail_if_id()}
              </Field.Description>
              <Button class="mt-2" type="button" variant="ghost" size="sm" onclick={() => onUseInventory(target)}>
                {m.model_editor_choose_synced_model()}
              </Button>
            {:else}
              {@const candidates = modelCandidates(target)}
              <SearchCombobox
                id={`target-model-${target.key}`}
                value={target.model}
                options={candidates.map(idNameOption)}
                placeholder={m.model_editor_choose_model()}
                emptyText={m.common_no_matching_models()}
                ariaLabel={m.model_editor_destination_value_model({ index: index + 1 })}
                disabled={!target.providerId || target.loading || candidates.length === 0}
                onValueChange={(value: string) => void onSelectModel(target, value)} />
              {#if target.providerId && !target.loading && !target.validationError && candidates.length === 0}
                <p class="text-sm text-warning" role="status">
                  {purpose ? m.route_purpose_no_candidates() : m.route_purpose_placeholder()}
                </p>
              {:else if purpose && candidates.length > 0 && candidates.length < target.inventory.length}
                <Field.Description>
                  {m.route_purpose_candidates_filtered({ purpose: routePurposeLabels[purpose]() })}
                </Field.Description>
              {/if}
            {/if}
          </Field.Field>
        </Field.Group>

        {#if target.loading}
          <div class="flex items-center gap-2 border-t pt-4 text-sm text-muted-foreground">
            <Spinner />{m.model_editor_loading_models_supported_features()}
          </div>
        {:else if summary}
          <div class="flex flex-wrap items-center gap-2 border-t pt-4">
            <ModelSpecification specification={summary.specification} />
            <ModelDetailsDialog
              providerId={target.providerId}
              modelId={target.model ?? ''}
              triggerLabel={m.model_editor_view_model_details()} />
          </div>
        {/if}

        {#if showConfigModePanel}
          <div
            class={[
              'flex flex-col gap-3 rounded-lg border p-3',
              target.configMode === 'custom'
                ? 'border-warning/40 bg-warning/[0.04]'
                : 'border-border bg-muted/30',
            ]}>
            <div class="flex flex-wrap items-center justify-between gap-2">
              <span id={`target-config-mode-label-${target.key}`} class="text-sm font-medium text-foreground">
                {m.model_editor_config_mode()}
              </span>
              <SegmentedControl
                options={configModeOptions}
                value={target.configMode}
                onValueChange={(mode: TargetConfigMode) => handleConfigModeChange(target, mode)}
                ariaLabelledby={`target-config-mode-label-${target.key}`} />
            </div>

            {#if isInheritedReadOnly}
              <p class="text-xs text-muted-foreground">
                {m.model_editor_config_mode_shared_help({
                  model: currentModelName,
                  count: groupMemberCount,
                })}
              </p>

              <div class="rounded-md border border-border bg-card p-3">
                <div class="mb-2 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
                  <LockIcon class="size-3.5" aria-hidden="true" />
                  <span>{m.model_editor_inherited_settings()}</span>
                </div>
                <dl class="grid grid-cols-2 gap-x-4 gap-y-2 text-xs sm:grid-cols-4">
                  <div>
                    <dt class="text-muted-foreground">{m.model_editor_first_token_timeout()}</dt>
                    <dd class="mt-0.5 font-technical font-medium text-foreground tabular-nums">
                      {inheritedSummary.timeoutSeconds}{m.model_editor_unit_seconds()}
                    </dd>
                  </div>
                  <div>
                    <dt class="text-muted-foreground">{m.model_editor_target_retry_budget()}</dt>
                    <dd class="mt-0.5 font-technical font-medium text-foreground tabular-nums">
                      {m.model_editor_summary_retries({ count: inheritedSummary.retryBudget })}
                    </dd>
                  </div>
                  <div>
                    <dt class="text-muted-foreground">{m.model_editor_target_cooldown()}</dt>
                    <dd class="mt-0.5 font-technical font-medium text-foreground tabular-nums">
                      {inheritedSummary.cooldownSeconds}{m.model_editor_unit_seconds()}
                    </dd>
                  </div>
                  <div>
                    <dt class="text-muted-foreground">{m.model_editor_thinking_map()}</dt>
                    <dd class="mt-0.5 font-technical font-medium text-foreground">
                      {formatThinkingLevelsSummary(inheritedSummary.enabledThinkingLevels)}
                    </dd>
                  </div>
                </dl>
              </div>

              <div class="flex flex-wrap items-center gap-2">
                {#if !isNew}
                  <Button type="button" variant="outline" size="sm" onclick={onOpenGroupConfig}>
                    <SlidersHorizontalIcon data-icon="inline-start" />
                    {m.model_editor_edit_group_config()}
                  </Button>
                {/if}
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onclick={() => handleConfigModeChange(target, 'custom')}>
                  {m.model_editor_switch_to_custom()}
                </Button>
              </div>
            {:else}
              <p class="text-xs text-muted-foreground">
                {m.model_editor_config_mode_custom_help({ provider: currentProviderName })}
              </p>
              <div class="flex flex-wrap items-center gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onclick={() => handleConfigModeChange(target, 'shared')}>
                  <RotateCcwIcon data-icon="inline-start" />
                  {m.model_editor_switch_to_shared()}
                </Button>
                {#if hasGroupPeers && !isNew}
                  <Button type="button" variant="ghost" size="sm" onclick={onOpenGroupConfig}>
                    <SlidersHorizontalIcon data-icon="inline-start" />
                    {m.model_editor_edit_group_config()}
                  </Button>
                {/if}
              </div>
            {/if}
          </div>
        {/if}

        {#if !isInheritedReadOnly}
          <Field.Group class="grid gap-4 border-t pt-4 sm:grid-cols-3">
            <Field.Field orientation="vertical" data-invalid={firstTokenInvalid ? 'true' : undefined}>
              <Field.Label
                for={`target-first-token-timeout-${target.key}`}
                hint={m.model_editor_first_token_timeout_help()}>
                {m.model_editor_first_token_timeout()}
              </Field.Label>
              <InputGroup.Root class="w-28">
                <InputGroup.Input
                  id={`target-first-token-timeout-${target.key}`}
                  type="number"
                  min="0"
                  step="0.001"
                  aria-invalid={firstTokenInvalid}
                  aria-describedby={firstTokenInvalid
                    ? `target-first-token-timeout-constraint-${target.key}`
                    : undefined}
                  bind:value={target.firstTokenTimeoutSeconds} />
                <InputGroup.Addon align="inline-end">
                  <InputGroup.Text>{m.model_editor_unit_seconds()}</InputGroup.Text>
                </InputGroup.Addon>
              </InputGroup.Root>
              {#if firstTokenInvalid}
                <Field.Error id={`target-first-token-timeout-constraint-${target.key}`}>
                  {m.model_editor_seconds_constraint()}
                </Field.Error>
              {/if}
            </Field.Field>

            <Field.Field orientation="vertical" data-invalid={retryBudgetInvalid ? 'true' : undefined}>
              <Field.Label
                for={`target-retry-budget-${target.key}`}
                hint={m.model_editor_target_retry_budget_help()}>
                {m.model_editor_target_retry_budget()}
              </Field.Label>
              <InputGroup.Root class="w-28">
                <InputGroup.Input
                  id={`target-retry-budget-${target.key}`}
                  type="number"
                  min="0"
                  step="1"
                  aria-invalid={retryBudgetInvalid}
                  aria-describedby={retryBudgetInvalid ? `target-retry-budget-constraint-${target.key}` : undefined}
                  bind:value={target.targetRetryBudget} />
                <InputGroup.Addon align="inline-end">
                  <InputGroup.Text>{m.model_editor_unit_times()}</InputGroup.Text>
                </InputGroup.Addon>
              </InputGroup.Root>
              {#if retryBudgetInvalid}
                <Field.Error id={`target-retry-budget-constraint-${target.key}`}>
                  {m.model_editor_retry_budget_constraint()}
                </Field.Error>
              {/if}
            </Field.Field>

            <Field.Field orientation="vertical" data-invalid={cooldownInvalid ? 'true' : undefined}>
              <Field.Label for={`target-cooldown-${target.key}`} hint={m.model_editor_target_cooldown_help()}>
                {m.model_editor_target_cooldown()}
              </Field.Label>
              <InputGroup.Root class="w-28">
                <InputGroup.Input
                  id={`target-cooldown-${target.key}`}
                  type="number"
                  min="0"
                  step="0.001"
                  aria-invalid={cooldownInvalid}
                  aria-describedby={cooldownInvalid ? `target-cooldown-constraint-${target.key}` : undefined}
                  bind:value={target.targetCooldownSeconds} />
                <InputGroup.Addon align="inline-end">
                  <InputGroup.Text>{m.model_editor_unit_seconds()}</InputGroup.Text>
                </InputGroup.Addon>
              </InputGroup.Root>
              {#if cooldownInvalid}
                <Field.Error id={`target-cooldown-constraint-${target.key}`}>
                  {m.model_editor_seconds_constraint()}
                </Field.Error>
              {/if}
            </Field.Field>
          </Field.Group>

          {#if !target.loading && target.providerId && target.model?.trim() && target.thinkingLevelMap.length > 0}
            {@const context = thinkingContextFor(target, summary)}
            {@const writableKinds = writableThinkingControlKinds(context)}
            {@const candidates = effortCandidates(context)}
            <div class="border-t pt-4">
              <div class="mb-3 flex flex-wrap items-start justify-between gap-3">
                <div class="min-w-0">
                  <div class="flex items-center gap-2">
                    <h4 class="font-medium">{m.model_editor_thinking_map()}</h4>
                    <Field.Help hint={m.model_editor_thinking_map_help()} />
                  </div>
                  {#if target.id}
                    <p class="mt-1 text-sm text-muted-foreground">{m.model_editor_thinking_map_immediate()}</p>
                  {/if}
                </div>
                {#if target.id}
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onclick={() => void onRegenerateThinkingMap(target)}>
                    {m.model_editor_thinking_regenerate()}
                  </Button>
                {/if}
              </div>
              <div data-slot="thinking-map" class="divide-y border-y">
                {#each target.thinkingLevelMap as row (row.level)}
                  {@const kindWritable = writableKinds.includes(row.control.type)}
                  {@const rowWritable = thinkingControlWritable(row.control, context)}
                  {@const errorId = `thinking-error-${target.key}-${row.level}`}
                  <div
                    data-slot="thinking-map-row"
                    class="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-2 py-2 sm:grid-cols-[minmax(7.5rem,0.8fr)_minmax(0,2.2fr)_auto]">
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
                          value &&
                          changeThinkingControlKind(target, row, value as TargetThinkingControl['type'], context)}>
                        <Select.Trigger
                          class="w-36 shrink-0"
                          aria-invalid={!kindWritable}
                          aria-describedby={!kindWritable ? errorId : undefined}
                          aria-label={m.model_editor_thinking_control({ level: row.level })}>
                          {thinkingControlLabel(row.control.type)}
                        </Select.Trigger>
                        <Select.Content>
                          <Select.Group>
                            {#each thinkingControlKinds as type (type)}
                              <Select.Item value={type} disabled={!writableKinds.includes(type)}>
                                {thinkingControlLabel(type)}
                              </Select.Item>
                            {/each}
                          </Select.Group>
                        </Select.Content>
                      </Select.Root>
                      {#if row.control.type === 'effort'}
                        {#if candidates?.length}
                          {@const current = row.control.value}
                          <Select.Root
                            type="single"
                            value={current}
                            onValueChange={(value: string) => value && changeThinkingEffort(target, row, value)}>
                            <Select.Trigger
                              title={current}
                              class="min-w-0 flex-1 font-technical"
                              aria-invalid={!rowWritable}
                              aria-describedby={!rowWritable ? errorId : undefined}
                              aria-label={m.model_editor_thinking_upstream_level({ level: row.level })}>
                              <span class={['truncate', !current.trim() && 'font-sans text-muted-foreground']}>
                                {current.trim() ? current : m.model_editor_thinking_choose_level()}
                              </span>
                            </Select.Trigger>
                            <Select.Content class="max-w-[calc(100vw-2rem)]">
                              <Select.Group>
                                {#each candidates as effort (effort)}
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
                            readonly={candidates !== null}
                            aria-invalid={!rowWritable}
                            aria-describedby={!rowWritable ? errorId : undefined}
                            oninput={(event: Event) => changeThinkingEffort(target, row, inputValue(event))} />
                        {/if}
                      {/if}
                    </div>
                    {#if target.id}
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon"
                        class="col-start-2 row-start-1 justify-self-end sm:col-auto sm:row-auto"
                        aria-label={`${m.model_editor_thinking_reset_row()}: ${row.level}`}
                        title={m.model_editor_thinking_reset_row()}
                        onclick={() => void onResetThinkingRow(target, row.level)}>
                        <RotateCcwIcon />
                      </Button>
                    {/if}
                    {#if !rowWritable}
                      <p id={errorId} class="col-span-full text-sm wrap-anywhere text-destructive" role="status">
                        {!kindWritable
                          ? m.model_editor_thinking_row_unwritable()
                          : candidates
                            ? m.model_editor_thinking_effort_undeclared({ efforts: formatList(candidates) })
                            : m.model_editor_thinking_effort_required()}
                      </p>
                    {/if}
                  </div>
                {/each}
              </div>
            </div>
          {/if}
        {/if}

        {#if target.validationError}
          <p class="text-sm text-warning" role="status">{target.validationError}</p>
        {/if}
      </div>

      <div class="route-overlay-footer">
        <Button type="button" variant="outline" onclick={() => onClose(false)}>
          {m.common_cancel()}
        </Button>
        <Button
          type="button"
          disabled={target.loading || (target.enabled && !configured)}
          onclick={() => onClose(true)}>
          {m.common_confirm()}
        </Button>
        <p class="text-sm text-muted-foreground sm:order-first sm:mr-auto sm:self-center">
          {m.model_editor_target_draft_note()}
        </p>
      </div>
    </Sheet.Content>
  {/if}
</Sheet.Root>
