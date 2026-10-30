<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { tick } from 'svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatNumber } from '#lib/format.js'

import type { CredentialMatch, CredentialRule, CustomCredentialRule } from '#lib/types.js'

import PageHeader from '#lib/components/page-header.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import CredentialMatchItem from '#lib/components/credential-match-item.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import TimeWindowControls from '#lib/components/time-window-controls.svelte'
import * as Accordion from '#lib/components/ui/accordion/index.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import { renderSnippet } from '@tanstack/svelte-table'
import ListFilterIcon from '@lucide/svelte/icons/list-filter'
import HistoryIcon from '@lucide/svelte/icons/history'
import ScanTextIcon from '@lucide/svelte/icons/scan-text'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import InfoIcon from '@lucide/svelte/icons/info'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SearchIcon from '@lucide/svelte/icons/search'
import { Badge } from '#lib/components/ui/badge/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import CustomRuleSheet from './custom-rule-sheet.svelte'
import HitRecords from './hit-records.svelte'
import PlusIcon from '@lucide/svelte/icons/plus'
import * as Tabs from '#lib/components/ui/tabs/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'

import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRowPointerEvent,
} from '#lib/components/ui/data-table/index.js'

import {
  credentialRulesQuery,
  CUSTOM_CREDENTIAL_RULES_KEY,
  customCredentialRulesQuery,
} from '#lib/credential-protection-queries.js'
import {
  customHitWindow,
  hitWindowBounds,
  isValidHitRange,
  shiftHitWindow,
  toLocalDateTimeInput,
  type HitWindow,
} from '#lib/credential-hit-window.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import { Textarea } from '#lib/components/ui/textarea/index.js'

const settingKey = 'reversible_redaction_enabled'
const queryKey = ['setting', settingKey]

// 内置与自定义规则在同一张表浏览；详情与编辑各自沿用唯一的表面。
type RuleRow =
  | { origin: 'builtin'; id: string; name: string; rule: CredentialRule }
  | { origin: 'custom'; id: string; name: string; rule: CustomCredentialRule }

const queryClient = useQueryClient()
const settingQuery = createQuery(() => ({ queryKey, queryFn: () => admin.settings.get(settingKey) }))

const rulesQuery = createQuery(() => credentialRulesQuery())
const customRulesQuery = createQuery(() => customCredentialRulesQuery())
let saving = $state(false)
let saveError = $state('')
const storedEnabled = $derived(settingQuery.data === 'true')
let tab = $state('records')
let ruleOpen = $state(false)
let detailsOpen = $state(false)
let selectedRule = $state.raw<CredentialRule>()
let customSheet = $state<{ show: (rule: CustomCredentialRule | null) => void }>()
let originFilter = $state('all')
let ruleSearch = $state('')
let pagination = $state({ pageIndex: 0, pageSize: 25 })
const rules = $derived(rulesQuery.data?.rules ?? [])
const customRules = $derived(customRulesQuery.data ?? [])
const ruleRows = $derived<RuleRow[]>([
  ...customRules.map((rule): RuleRow => ({ origin: 'custom', id: rule.id, name: rule.name, rule })),
  ...rules.map((rule): RuleRow => ({ origin: 'builtin', id: rule.id, name: rule.name, rule })),
])
const visibleRows = $derived.by(() => {
  const needle = ruleSearch.trim().toLocaleLowerCase()
  return ruleRows.filter(
    (row) =>
      (originFilter === 'all' || row.origin === originFilter) &&
      (!needle || ruleSearchText(row).toLocaleLowerCase().includes(needle)),
  )
})
const ruleNames = $derived(new Map(ruleRows.map((row) => [row.id, row.name])))
const ruleRowById = $derived(new Map(ruleRows.map((row) => [row.id, row])))
const originFilterLabel = $derived(
  originFilter === 'builtin'
    ? m.credential_custom_filter_builtin()
    : originFilter === 'custom'
      ? m.credential_custom_filter_custom()
      : m.credential_custom_filter_all(),
)
const tableLabels = $derived({
  ...getDataTableLabels(),
  rowsPerPage: m.credential_protection_rows_per_page(),
  pageStatus: (page: number, pageCount: number) => m.logs_pagination({ pageIndex: page, pageCount }),
  firstPage: m.credential_protection_first_page(),
  previousPage: m.logs_previous(),
  nextPage: m.logs_next(),
  lastPage: m.credential_protection_last_page(),
  loading: m.credential_protection_catalog_loading(),
})
const ruleColumn = createDataTableColumnHelper<RuleRow>()
const ruleColumns = ruleColumn.columns([
  ruleColumn.accessor(ruleSearchText, {
    id: 'rule',
    header: () => m.credential_protection_rule_name(),
    meta: { label: () => m.credential_protection_rule_name(), width: 'compact' },
    cell: (context) => renderSnippet(ruleNameCell, context),
    size: 288,
  }),
  ruleColumn.accessor((row) => (row.origin === 'builtin' ? row.rule.keywords.join(' ') : ''), {
    id: 'conditions',
    header: () => m.credential_protection_rule_conditions(),
    meta: { label: () => m.credential_protection_rule_conditions(), width: 'fill' },
    cell: (context) => renderSnippet(ruleConditionsCell, context),
    enableSorting: false,
    size: 320,
  }),
])

// 自定义规则的匹配文本可能就是凭据本身，不参与搜索索引。
function ruleSearchText(row: RuleRow): string {
  return row.origin === 'builtin'
    ? `${row.name} ${row.id} ${row.rule.target} ${row.rule.description} ${row.rule.keywords.join(' ')}`
    : `${row.name} ${row.id} ${row.rule.description}`
}

const HOUR_MS = 3_600_000
const windowPresets = $derived([
  { value: String(6 * HOUR_MS), label: m.stats_last_6h() },
  { value: String(24 * HOUR_MS), label: m.stats_last_24h() },
  { value: String(72 * HOUR_MS), label: m.stats_last_3d() },
  { value: String(168 * HOUR_MS), label: m.stats_last_7d() },
])
// 默认 7 天与观察记录的默认保留期一致，打开页面就能看到全部保留的命中。
let hitWindow = $state.raw<HitWindow>({ live: true, custom: false, spanMs: 168 * HOUR_MS })
let hitRecords = $state<{ reload: () => void }>()
// 实时窗口的起点随时间推移，时间条每秒跟进一次；固定窗口不需要。
let now = $state(Date.now())
let rangeOpen = $state(false)
let draftStart = $state('')
let draftEnd = $state('')
const hitBounds = $derived(hitWindowBounds(hitWindow, now))
const windowPresetValue = $derived(hitWindow.custom ? '' : String(hitWindow.spanMs))
const windowSelectedLabel = $derived(
  windowPresets.find((preset) => preset.value === windowPresetValue)?.label ?? m.observation_custom_range(),
)
const draftStartMs = $derived(new Date(draftStart).getTime())
const draftEndMs = $derived(new Date(draftEnd).getTime())
const validDraftRange = $derived(isValidHitRange(draftStartMs, draftEndMs))

$effect(() => {
  if (tab !== 'records' || !hitWindow.live) return
  now = Date.now()
  const timer = setInterval(() => (now = Date.now()), 1000)
  return () => clearInterval(timer)
})

function openRange(): void {
  draftStart = toLocalDateTimeInput(hitBounds.startAt)
  draftEnd = toLocalDateTimeInput(hitBounds.endAt)
  rangeOpen = true
}

function applyRange(): void {
  if (!validDraftRange) return
  rangeOpen = false
  hitWindow = customHitWindow(draftStartMs, draftEndMs)
}

function patternKeywords(rule: CustomCredentialRule): string[] {
  return rule.spec.mode === 'pattern' ? rule.spec.keywords : []
}

function openRule(row: RuleRow): void {
  if (row.origin === 'custom') {
    customSheet?.show(row.rule)
    return
  }
  selectedRule = row.rule
  ruleOpen = true
}

let testInput = $state('')
let inputElement = $state<HTMLTextAreaElement | null>(null)
let testing = $state(false)
let testError = $state(false)
let matches = $state.raw<CredentialMatch[]>()
let resultsPane = $state<HTMLElement | null>(null)

async function setEnabled(enabled: boolean): Promise<void> {
  if (!settingQuery.isSuccess || saving || enabled === storedEnabled) return
  const value = enabled ? 'true' : 'false'
  saving = true
  saveError = ''
  try {
    await admin.settings.set(settingKey, value)
    queryClient.setQueryData(queryKey, value)
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
  } finally {
    saving = false
  }
}

function componentScope(within: string): string {
  if (!within) return m.credential_protection_component_same_text()
  return within
    .split(',')
    .map((part) => {
      const distance = Number.parseInt(part, 10)
      if (part.endsWith('L')) return m.credential_protection_component_lines({ count: distance - 1 })
      if (part.endsWith('C')) return m.credential_protection_component_columns({ count: distance })
      return part
    })
    .join(' · ')
}

function confidenceLabel(confidence: string): string {
  switch (confidence) {
    case 'high':
      return m.credential_protection_confidence_high()
    case 'medium':
      return m.credential_protection_confidence_medium()
    case 'low':
      return m.credential_protection_confidence_low()
    default:
      return confidence
  }
}

async function testText(): Promise<void> {
  if (testing || !testInput.length) return
  testing = true
  testError = false
  matches = undefined
  try {
    matches = (await admin.credentialProtection.test(testInput)).matches
  } catch {
    testError = true
  } finally {
    testing = false
  }
  // 窄屏结果堆叠在操作之后：运行结束后让结果落入视口。
  await tick()
  resultsPane?.scrollIntoView({ block: 'nearest', behavior: 'instant' })
}

function clearTest(): void {
  testInput = ''
  matches = undefined
  testError = false
  inputElement?.focus()
}

function selectMatch(match: CredentialMatch): void {
  inputElement?.focus()
  inputElement?.setSelectionRange(match.start, match.end)
  inputElement?.scrollIntoView({ block: 'center', behavior: 'instant' })
}
</script>

<svelte:head><title>{m.reversible_redaction_title()} · Stravia</title></svelte:head>

{#snippet pageActions()}
  <Button
    variant="outline"
    onclick={() => {
      detailsOpen = true
    }}>
    <InfoIcon data-icon="inline-start" />{m.credential_protection_details()}
  </Button>
{/snippet}

{#snippet ruleNameCell(context: DataTableCellContext<RuleRow>)}
  {@const row = context.row.original}
  <button type="button" class="rule-link" onclick={() => openRule(row)}>
    <span class="flex flex-wrap items-center gap-2">
      <span class="font-medium">{row.name}</span>
      {#if row.origin === 'custom'}
        <Badge variant="secondary">{m.credential_custom_badge()}</Badge>
        {#if !row.rule.enabled}<Badge variant="outline">{m.common_disabled_status()}</Badge>{/if}
      {/if}
    </span>
  </button>
{/snippet}

{#snippet customConditions(rule: CustomCredentialRule)}
  {@const spec = rule.spec}
  <div class="flex flex-wrap items-center gap-1.5">
    {#if spec.mode === 'simple'}
      <Badge variant="outline">{m.credential_custom_simple_condition()}</Badge>
    {:else}
      {@const keywords = patternKeywords(rule)}
      {#each keywords.slice(0, 3) as keyword (keyword)}
        <Badge variant="secondary" class="h-auto max-w-full"
          ><code class="whitespace-normal break-all">{keyword}</code></Badge>
      {/each}
      {#if keywords.length > 3}
        <span class="text-xs text-muted-foreground">+{formatNumber(keywords.length - 3)}</span>
      {/if}
      <Badge variant="outline">{m.credential_custom_pattern_condition()}</Badge>
    {/if}
  </div>
{/snippet}

{#snippet builtinConditions(rule: CredentialRule)}
  <div class="flex flex-wrap items-center gap-1.5">
    {#each rule.keywords.slice(0, 3) as keyword (keyword)}
      <Badge variant="secondary" class="h-auto max-w-full"
        ><code class="whitespace-normal break-all">{keyword}</code></Badge>
    {/each}
    {#if rule.keywords.length > 3}
      <span class="text-xs text-muted-foreground">+{formatNumber(rule.keywords.length - 3)}</span>
    {/if}
    {#if rule.path}<Badge variant="outline">{m.credential_protection_path_rule()}</Badge>{/if}
    {#if rule.components.length}<Badge variant="outline">{m.credential_protection_composite_rule()}</Badge>{/if}
    {#if rule.skip_report}<Badge variant="outline">{m.credential_protection_component_only()}</Badge>{/if}
    {#if !rule.keywords.length && !rule.path && !rule.components.length && !rule.skip_report}
      <span class="text-xs text-muted-foreground">{m.credential_protection_no_keywords()}</span>
    {/if}
  </div>
{/snippet}

{#snippet ruleConditionsCell(context: DataTableCellContext<RuleRow>)}
  {@const row = context.row.original}
  {#if row.origin === 'custom'}
    {@render customConditions(row.rule)}
  {:else}
    {@render builtinConditions(row.rule)}
  {/if}
{/snippet}

<div class={['route-page', { 'table-workspace': tab !== 'test' }]}>
  <PageHeader
    title={m.reversible_redaction_title()}
    description={m.credential_protection_scope_brief()}
    actions={pageActions}>
    {#snippet meta()}
      <div class="flex items-center gap-2">
        {#if settingQuery.isPending}
          <span class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</span>
        {:else if saving}
          <Spinner aria-label={m.settings_saving()} />
        {/if}
        <Switch
          id="reversible-redaction-enabled"
          bind:checked={() => storedEnabled, (checked: boolean) => void setEnabled(checked)}
          disabled={!settingQuery.isSuccess || saving}
          aria-busy={saving}
          aria-label={m.reversible_redaction_enable()} />
        <span class="text-sm font-medium text-muted-foreground" aria-hidden="true"
          >{storedEnabled ? m.common_enabled_status() : m.common_disabled_status()}</span>
      </div>
    {/snippet}
  </PageHeader>

  {#if settingQuery.isError || saveError}
    <section class="route-section flex flex-col gap-3" aria-label={m.reversible_redaction_enable()}>
      {#if settingQuery.isError}
        <Alert.Root variant="destructive"
          ><Alert.Description>{m.reversible_redaction_load_failed()}</Alert.Description></Alert.Root>
        <Button class="self-start" variant="outline" onclick={() => void settingQuery.refetch()}
          >{m.common_retry()}</Button>
      {/if}
      {#if saveError}<Alert.Root variant="destructive"><Alert.Description>{saveError}</Alert.Description></Alert.Root
        >{/if}
    </section>
  {/if}

  <Tabs.Root
    bind:value={tab}
    class="credential-workspace relative min-h-0 min-w-0 gap-0 rounded-lg border bg-background">
    <div class="workspace-toolbar">
      <Tabs.List aria-label={m.reversible_redaction_title()}>
        <Tabs.Trigger value="records"><HistoryIcon />{m.credential_protection_records_tab()}</Tabs.Trigger>
        <Tabs.Trigger value="rules"
          ><ListFilterIcon />{m.credential_protection_catalog_title()}
          {#if rulesQuery.isSuccess}<Badge variant="secondary">{formatNumber(ruleRows.length)}</Badge>{/if}
        </Tabs.Trigger>
        <Tabs.Trigger value="test"><ScanTextIcon />{m.credential_protection_test_tab()}</Tabs.Trigger>
      </Tabs.List>
      {#if tab === 'records'}
        <TimeWindowControls
          presets={windowPresets}
          value={windowPresetValue}
          selectedLabel={windowSelectedLabel}
          startMs={hitBounds.startAt}
          endMs={hitBounds.endAt}
          live={hitWindow.live}
          selectLabel={m.credential_protection_records_window_label()}
          rangeLabel={m.observation_choose_range()}
          olderLabel={m.observation_older_window()}
          newerLabel={m.observation_newer_window()}
          newerDisabled={hitWindow.live}
          onpreset={(value: string) => (hitWindow = { live: true, custom: false, spanMs: Number(value) })}
          onolder={() => (hitWindow = shiftHitWindow(hitWindow, -1, Date.now()))}
          onnewer={() => (hitWindow = shiftHitWindow(hitWindow, 1, Date.now()))}
          onrange={openRange}>
          {#snippet trailing()}
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label={m.credential_protection_records_refresh()}
              onclick={() => hitRecords?.reload()}><RefreshCwIcon /></Button>
          {/snippet}
        </TimeWindowControls>
      {:else if tab === 'rules'}
        <div class="toolbar-actions" data-slot="credential-rules-toolbar">
          {#if !rulesQuery.isError}
            <InputGroup.Root class="w-full min-w-0 sm:w-auto sm:max-w-72 sm:flex-1 sm:basis-48">
              <InputGroup.Input
                id="credential-rule-search"
                bind:value={ruleSearch}
                aria-label={m.credential_protection_catalog_search()}
                placeholder={m.credential_protection_catalog_search()}
                oninput={() => (pagination = { ...pagination, pageIndex: 0 })} />
              <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
            </InputGroup.Root>
            <Select.Root
              type="single"
              bind:value={originFilter}
              onValueChange={() => (pagination = { ...pagination, pageIndex: 0 })}>
              <Select.Trigger class="w-36" aria-label={m.credential_custom_filter_label()}
                >{originFilterLabel}</Select.Trigger>
              <Select.Content>
                <Select.Group>
                  <Select.Item value="all">{m.credential_custom_filter_all()}</Select.Item>
                  <Select.Item value="builtin">{m.credential_custom_filter_builtin()}</Select.Item>
                  <Select.Item value="custom">{m.credential_custom_filter_custom()}</Select.Item>
                </Select.Group>
              </Select.Content>
            </Select.Root>
          {/if}
          <Button onclick={() => customSheet?.show(null)}>
            <PlusIcon data-icon="inline-start" />{m.credential_custom_add()}
          </Button>
        </div>
      {/if}
    </div>

    <Tabs.Content value="records" class="flex min-h-0 min-w-0 flex-col">
      <section class="workspace-panel" aria-labelledby="credential-discoveries-title">
        <h2 id="credential-discoveries-title" class="sr-only">
          {m.credential_protection_discoveries_title()}
        </h2>
        <HitRecords
          bind:this={hitRecords}
          active={tab === 'records'}
          range={hitWindow}
          {ruleNames}
          rulesPending={rulesQuery.isPending || customRulesQuery.isPending}
          onOpenRule={(ruleId: string) => {
            const row = ruleRowById.get(ruleId)
            if (row) openRule(row)
          }}
          onTest={() => {
            tab = 'test'
          }} />
      </section>
    </Tabs.Content>

    <Tabs.Content value="rules" class="flex min-h-0 min-w-0 flex-col">
      <section class="workspace-panel rules-panel" aria-labelledby="credential-rules-title">
        <h2 id="credential-rules-title" class="sr-only">{m.credential_protection_catalog_title()}</h2>
        {#if rulesQuery.isError}
          <div class="panel-state">
            <RequestFailure
              class="w-full max-w-xl"
              message={m.credential_protection_catalog_failed()}
              retry={() => rulesQuery.refetch()} />
          </div>
        {:else}
          {#if customRulesQuery.isError}
            <div class="panel-notice">
              <RequestFailure
                message={m.credential_custom_load_failed()}
                retry={() => customRulesQuery.refetch()} />
            </div>
          {/if}
          <DataTable
            workbench
            class="min-h-0 flex-1"
            data={visibleRows}
            columns={ruleColumns}
            labels={tableLabels}
            getRowId={(row: RuleRow) => row.id}
            ariaLabel={m.credential_protection_catalog_title()}
            size="large"
            stickyHeader
            loading={rulesQuery.isPending}
            loadingRows={8}
            paginator
            bind:pagination
            pageSizeOptions={[10, 25, 50]}
            onRowClick={({ row }: DataTableRowPointerEvent<RuleRow>) => openRule(row.original)}>
            {#snippet empty()}
              <Empty.Root
                ><Empty.Header
                  ><Empty.Media variant="icon"><ListFilterIcon /></Empty.Media>
                  <Empty.Title>{m.credential_protection_catalog_title()}</Empty.Title>
                  <Empty.Description
                    >{originFilter === 'custom' && !customRules.length
                      ? m.credential_custom_empty()
                      : ruleRows.length
                        ? m.credential_protection_catalog_no_results()
                        : m.credential_protection_catalog_empty()}</Empty.Description>
                </Empty.Header>
              </Empty.Root>
            {/snippet}
          </DataTable>
        {/if}
      </section>
    </Tabs.Content>

    <Tabs.Content value="test" class="min-w-0">
      <section class="workspace-panel" aria-labelledby="credential-tester-title">
        <h2 id="credential-tester-title" class="sr-only">{m.credential_protection_tester_title()}</h2>
        <div class="test-workspace">
          <div class="test-input-pane">
            <form
              onsubmit={(event) => {
                event.preventDefault()
                void testText()
              }}
              autocomplete="off">
              <Field.Group>
                <Field.Field orientation="vertical" data-disabled={testing}>
                  <Field.Label for="credential-test-input">{m.credential_protection_tester_input()}</Field.Label>
                  <Textarea
                    id="credential-test-input"
                    bind:ref={inputElement}
                    bind:value={testInput}
                    class="min-h-40 max-h-[36rem] resize-y font-mono @3xl/route-page:min-h-72"
                    spellcheck={false}
                    autocomplete="off"
                    autocapitalize="off"
                    disabled={testing}
                    aria-describedby="credential-test-flow"
                    oninput={() => {
                      matches = undefined
                      testError = false
                    }} />
                </Field.Field>
                <Field.Field orientation="horizontal" class="flex-wrap">
                  <Button type="submit" disabled={testing || !testInput.length} aria-busy={testing}
                    >{#if testing}<Spinner data-icon="inline-start" />{/if}{testing
                      ? m.credential_protection_tester_running()
                      : m.credential_protection_tester_submit()}</Button>
                  <Button type="button" variant="outline" disabled={testing} onclick={clearTest}
                    >{m.credential_protection_tester_clear()}</Button>
                </Field.Field>
              </Field.Group>
            </form>
            <p id="credential-test-flow" class="mt-4 text-xs leading-relaxed text-muted-foreground">
              {m.credential_protection_tester_flow()}
            </p>
          </div>
          <div
            bind:this={resultsPane}
            class="test-results-pane"
            role="region"
            aria-labelledby="test-results-title"
            aria-busy={testing}>
            <h3 id="test-results-title" class="mb-4 text-sm font-medium">{m.credential_protection_results_title()}</h3>
            {#if testing}
              <Empty.Root
                ><Empty.Header
                  ><Empty.Media><Spinner /></Empty.Media><Empty.Description role="status"
                    >{m.credential_protection_tester_running()}</Empty.Description
                  ></Empty.Header
                ></Empty.Root>
            {:else if matches === undefined && !testError}
              <Empty.Root class="min-h-64"
                ><Empty.Header
                  ><Empty.Media variant="icon"><ScanTextIcon /></Empty.Media>
                  <Empty.Title>{m.credential_protection_test_ready()}</Empty.Title>
                  <Empty.Description>{m.credential_protection_test_ready_hint()}</Empty.Description>
                </Empty.Header></Empty.Root>
            {/if}
            {#if testError}<Alert.Root variant="destructive"
                ><Alert.Description>{m.credential_protection_tester_failed()}</Alert.Description></Alert.Root
              >{/if}
            {#if matches}
              <div role="status" class="text-sm">
                {matches.length
                  ? m.credential_protection_tester_matches({ count: formatNumber(matches.length) })
                  : m.credential_protection_tester_none()}
              </div>
              <ul class="flex flex-col gap-2">
                {#each matches as match (`${match.rule_id}:${match.start}:${match.end}`)}
                  <CredentialMatchItem {match} {ruleNames} onselect={selectMatch} />
                {/each}
              </ul>
            {/if}
          </div>
        </div>
      </section>
    </Tabs.Content>
  </Tabs.Root>
</div>

<Dialog.Root bind:open={rangeOpen}>
  <Dialog.Content>
    <Dialog.Header>
      <div class="flex items-center gap-1.5">
        <Dialog.Title>{m.observation_date_time_range()}</Dialog.Title>
        <Field.Help hint={m.observation_range_local_time()} />
      </div>
      <Dialog.Description>{m.credential_protection_records_range_help()}</Dialog.Description>
    </Dialog.Header>
    <form
      class="flex flex-col gap-4"
      onsubmit={(event) => {
        event.preventDefault()
        applyRange()
      }}>
      <Field.FieldGroup>
        <Field.Field data-invalid={!validDraftRange || undefined}>
          <Field.FieldLabel for="credential-range-start">{m.observation_start_time()}</Field.FieldLabel>
          <Input
            id="credential-range-start"
            type="datetime-local"
            step="1"
            required
            bind:value={draftStart}
            aria-invalid={!validDraftRange}
            aria-describedby={validDraftRange ? undefined : 'credential-range-help'} />
        </Field.Field>
        <Field.Field data-invalid={!validDraftRange || undefined}>
          <Field.FieldLabel for="credential-range-end">{m.observation_end_time()}</Field.FieldLabel>
          <Input
            id="credential-range-end"
            type="datetime-local"
            step="1"
            required
            bind:value={draftEnd}
            aria-invalid={!validDraftRange}
            aria-describedby={validDraftRange ? undefined : 'credential-range-help'} />
        </Field.Field>
      </Field.FieldGroup>
      {#if !validDraftRange}
        <p id="credential-range-help" class="text-sm text-destructive" role="alert">
          {m.credential_protection_records_range_invalid()}
        </p>
      {/if}
      <Dialog.Footer>
        <Button variant="outline" onclick={() => (rangeOpen = false)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={!validDraftRange}>{m.observation_apply_range()}</Button>
      </Dialog.Footer>
    </form>
  </Dialog.Content>
</Dialog.Root>

<CustomRuleSheet
  bind:this={customSheet}
  onchanged={() => void queryClient.invalidateQueries({ queryKey: CUSTOM_CREDENTIAL_RULES_KEY })} />

<Sheet.Root bind:open={ruleOpen}>
  <Sheet.Content
    closeLabel={m.common_close()}
    class="data-[side=right]:w-full data-[side=right]:sm:max-w-xl overflow-y-auto">
    <Sheet.Header class="pr-12">
      <Sheet.Title>{selectedRule?.name ?? m.credential_protection_catalog_title()}</Sheet.Title>
      <Sheet.Description>{selectedRule?.description}</Sheet.Description>
      {#if selectedRule}<TechnicalValue value={selectedRule.id} copyable />{/if}
    </Sheet.Header>
    {#if selectedRule}
      <div class="flex min-w-0 flex-col gap-5 px-4 pb-6">
        <section aria-labelledby="rule-keywords-title">
          <div class="mb-2 flex items-center gap-2">
            <h3 id="rule-keywords-title" class="text-sm font-medium">{m.credential_protection_keywords()}</h3>
            {#if selectedRule.keywords.length}
              <Field.Help
                hint={selectedRule.skip_report
                  ? m.credential_protection_component_keywords_help()
                  : m.credential_protection_keywords_help()} />
            {/if}
          </div>
          {#if selectedRule.keywords.length}
            <div class="flex flex-wrap gap-1.5">
              {#each selectedRule.keywords as keyword (keyword)}
                <Badge variant="secondary" class="h-auto max-w-full"
                  ><code class="whitespace-normal break-all">{keyword}</code></Badge>
              {/each}
            </div>
          {:else}
            <p class="text-sm text-muted-foreground">{m.credential_protection_no_keywords()}</p>
          {/if}
        </section>
        <section>
          <h3 class="mb-2 text-sm font-medium">{m.credential_protection_rule_expression()}</h3>
          <pre class="rule-code">{selectedRule.regex ?? m.credential_protection_rule_no_expression()}</pre>
        </section>
        {#if selectedRule.filter.trim()}
          <section aria-labelledby="rule-exclusions-title">
            <div class="mb-2 flex items-center gap-2">
              <h3 id="rule-exclusions-title" class="text-sm font-medium">{m.credential_protection_exclusions()}</h3>
              <Field.Help hint={m.credential_protection_exclusions_help()} />
            </div>
            <pre class="rule-code">{selectedRule.filter.trim()}</pre>
          </section>
        {/if}
        {#if selectedRule.path}
          <section aria-labelledby="rule-path-title">
            <h3 id="rule-path-title" class="mb-2 text-sm font-medium">{m.credential_protection_path_condition()}</h3>
            <pre class="rule-code">{selectedRule.path}</pre>
            <p class="mt-2 text-xs text-muted-foreground">{m.credential_protection_catalog_path_note()}</p>
          </section>
        {/if}
        {#if selectedRule.components.length}
          <section aria-labelledby="rule-components-title">
            <h3 id="rule-components-title" class="mb-2 text-sm font-medium">
              {m.credential_protection_composite_rule()}
            </h3>
            <ul class="divide-y rounded-md border px-3">
              {#each selectedRule.components as component (component.id)}
                <li class="flex flex-col gap-1 py-3">
                  <div class="flex flex-wrap items-center justify-between gap-2">
                    <span class="text-sm font-medium"
                      >{ruleNames.get(component.id) ?? m.credential_protection_component_unknown()}</span>
                    <Badge variant={component.optional ? 'secondary' : 'outline'}>
                      {component.optional
                        ? m.credential_protection_component_optional()
                        : m.credential_protection_component_required()}
                    </Badge>
                  </div>
                  <p class="text-xs text-muted-foreground">{componentScope(component.within)}</p>
                </li>
              {/each}
            </ul>
          </section>
        {/if}
        <Accordion.Root type="single">
          <Accordion.Item value="parameters">
            <Accordion.Trigger>{m.credential_protection_parameters()}</Accordion.Trigger>
            <Accordion.Content>
              <dl class="rule-parameters">
                <dt>{m.credential_protection_capture_group()}</dt>
                <dd>
                  {selectedRule.secret_group
                    ? m.credential_protection_capture_number({ group: selectedRule.secret_group })
                    : m.credential_protection_capture_auto()}
                </dd>
                <dt>{m.credential_protection_priority()}</dt>
                <dd class="font-mono">{formatNumber(selectedRule.specificity)}</dd>
                <dt class="flex items-center gap-1.5">
                  {m.credential_protection_confidence()}<Field.Help hint={m.credential_protection_confidence_help()} />
                </dt>
                <dd>{confidenceLabel(selectedRule.confidence)}</dd>
                <dt>{m.credential_protection_rule_usage()}</dt>
                <dd>
                  {selectedRule.skip_report
                    ? m.credential_protection_component_only()
                    : m.credential_protection_standalone()}
                </dd>
              </dl>
            </Accordion.Content>
          </Accordion.Item>
        </Accordion.Root>
        <Button
          variant="outline"
          class="self-start"
          onclick={() => {
            ruleOpen = false
            tab = 'test'
          }}>
          {m.credential_protection_test_tab()}<ArrowRightIcon data-icon="inline-end" />
        </Button>
      </div>
    {/if}
  </Sheet.Content>
</Sheet.Root>

<Sheet.Root bind:open={detailsOpen}>
  <Sheet.Content
    closeLabel={m.common_close()}
    class="data-[side=right]:w-full data-[side=right]:sm:max-w-xl overflow-y-auto">
    <Sheet.Header
      ><Sheet.Title>{m.credential_protection_details()}</Sheet.Title>
      <Sheet.Description>{m.reversible_redaction_summary()}</Sheet.Description>
    </Sheet.Header>
    <div class="flex flex-col gap-5 px-4 pb-6">
      <p class="text-sm text-muted-foreground">{m.reversible_redaction_scope()}</p>
      <div class="flex flex-col gap-3 text-sm text-muted-foreground">
        <p>{m.reversible_redaction_protection()}</p>
        <p>{m.reversible_redaction_mapping()}</p>
        <p>{m.reversible_redaction_failures()}</p>
        <p>{m.reversible_redaction_off()}</p>
        <p>{m.reversible_redaction_tools_warning()}</p>
        <p>{m.reversible_redaction_limits()}</p>
        <p>{m.credential_protection_hits_limits()}</p>
      </div>
      <Accordion.Root type="single">
        <Accordion.Item value="shared-conditions"
          ><Accordion.Trigger>{m.credential_protection_catalog_conditions()}</Accordion.Trigger>
          <Accordion.Content
            ><div class="flex flex-col gap-3">
              <p class="text-sm text-muted-foreground">{m.credential_protection_catalog_intro()}</p>
              <p class="text-sm text-muted-foreground">{m.credential_protection_catalog_path_note()}</p>
              <p class="text-sm text-muted-foreground">{m.credential_protection_rule_conditions_help()}</p>
              {#if rulesQuery.isSuccess}
                <pre class="rule-code">{JSON.stringify(
                    { prefilter: rulesQuery.data.prefilter, filter: rulesQuery.data.filter },
                    null,
                    2,
                  )}</pre>
              {:else}<p role="status">
                  {rulesQuery.isError
                    ? m.credential_protection_catalog_failed()
                    : m.credential_protection_catalog_loading()}
                </p>{/if}
            </div></Accordion.Content>
        </Accordion.Item>
      </Accordion.Root>
    </div>
  </Sheet.Content>
</Sheet.Root>

<style>
.table-workspace {
  /* 页头与可选的错误区段取自然高度，工作区吸收其余高度；不依赖兄弟节点数量定位行。 */
  display: flex;
  flex-direction: column;
  /* 预留标题栏、主内容内边距与桌面底部沟槽，其余高度交给工作区；超出时页面滚动并保留底部内边距。 */
  min-height: calc(100svh - 5rem);
}
@media (max-width: 767px) {
  .table-workspace {
    min-height: calc(100svh - 4.5rem);
  }
}
/* 工作区不以表格内容撑高，而是吸收剩余高度，表体在其中滚动。
 * 低窗口至少保留 24rem 的列表工作区（外加工具栏）；窄屏的工具栏与分页器各占多行，需要更多高度。
 * 命中记录与现有规则共用这套布局；匹配测试以自然高度显示。 */
.table-workspace > :global(.credential-workspace) {
  flex: 1 1 calc(24rem + 3.5rem + 2px);
  min-height: calc(24rem + 3.5rem + 2px);
  contain: size;
}
@media (max-width: 767px) {
  .table-workspace > :global(.credential-workspace) {
    flex-basis: calc(34rem + 7rem);
    min-height: calc(34rem + 7rem);
  }
}
.workspace-toolbar {
  display: flex;
  flex-shrink: 0;
  min-height: 3.5rem;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 0.5rem 1rem;
  border-bottom: 1px solid var(--border);
  padding: 0.55rem 0.75rem;
}
.toolbar-actions {
  display: flex;
  min-width: 0;
  max-width: 100%;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.5rem;
}
@media (max-width: 767px) {
  .workspace-toolbar {
    align-items: stretch;
    flex-direction: column;
  }
  .toolbar-actions {
    flex-direction: column;
    align-items: stretch;
  }
  .toolbar-actions :global([data-slot='select-trigger']) {
    width: 100%;
  }
}
.workspace-panel {
  display: flex;
  flex: 1 1 auto;
  flex-direction: column;
  min-height: 0;
  min-width: 0;
}
.panel-state {
  display: grid;
  flex: 1 1 auto;
  place-items: center;
  padding: 2rem 1rem;
}
.panel-notice {
  flex-shrink: 0;
  border-bottom: 1px solid var(--border);
  padding: 0.5rem 0.75rem;
}
/* 工作区外框已提供边界：规则表贴边，分页器与表体以细线分隔。
 * 表格滚动条默认画在表格盒外侧，贴边后改为叠在表格内。 */
.rules-panel :global([data-slot='data-table']) {
  gap: 0;
}
.rules-panel :global([data-slot='data-table-viewport']) {
  border: 0;
  border-radius: 0;
}
.rules-panel :global([data-scroll-area-scrollbar]) {
  translate: none;
}
.rules-panel :global([data-slot='data-table-footer']) {
  border-top: 1px solid var(--border);
  padding: 0.5rem 0.75rem;
}
.rule-link {
  display: flex;
  flex-direction: column;
  gap: 0.25rem;
  width: 100%;
  min-width: 0;
  cursor: pointer;
  text-align: start;
  white-space: normal;
  overflow-wrap: anywhere;
  border-radius: 0.2rem;
}
.rule-link:hover {
  color: var(--primary);
}
.rule-link:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: 4px;
}
.rule-parameters {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr);
  align-items: baseline;
  gap: 0.75rem 1.5rem;
  font-size: 0.8125rem;
}
.rule-parameters dt {
  color: var(--muted-foreground);
}
.rule-parameters dd {
  text-align: end;
}
.rule-code {
  max-width: 100%;
  overflow-x: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: var(--muted);
  padding: 1rem;
  font-family: var(--font-technical);
  font-size: 0.75rem;
  line-height: 1.7;
}
.test-workspace {
  display: grid;
  grid-template-columns: minmax(0, 1.15fr) minmax(0, 1fr);
  background: var(--card);
}
.test-input-pane {
  padding: 1rem;
  min-width: 0;
}
.test-results-pane {
  min-width: 0;
  padding: 1rem;
  border-left: 1px solid var(--border);
  background: color-mix(in oklch, var(--muted) 35%, var(--card));
}
@container route-page (max-width: 48rem) {
  .test-workspace {
    grid-template-columns: minmax(0, 1fr);
  }
  .test-results-pane {
    border-left: 0;
    border-top: 1px solid var(--border);
  }
}
</style>
