<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount } from 'svelte'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatLogTime, formatNumber } from '$lib/format'
import { observationStatusLabel } from '$lib/observation-labels'
import type { CredentialDiscoverySummary, CredentialMatch, CredentialRule } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import * as Accordion from '$lib/components/ui/accordion'
import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import { renderSnippet } from '@tanstack/svelte-table'
import ListFilterIcon from '@lucide/svelte/icons/list-filter'
import HistoryIcon from '@lucide/svelte/icons/history'
import ScanTextIcon from '@lucide/svelte/icons/scan-text'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import InfoIcon from '@lucide/svelte/icons/info'
import { Badge } from '$lib/components/ui/badge'
import * as Tabs from '$lib/components/ui/tabs'
import * as Sheet from '$lib/components/ui/sheet'
import { DataTable, createDataTableColumnHelper, type DataTableCellContext } from '$lib/components/ui/data-table'
import { getDataTableLabels } from '$lib/data-table-labels'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import { Textarea } from '$lib/components/ui/textarea'

const settingKey = 'reversible_redaction_enabled'
const queryKey = ['setting', settingKey]
const queryClient = useQueryClient()
const settingQuery = createQuery(() => ({ queryKey, queryFn: () => admin.settings.get(settingKey) }))
const rulesQuery = createQuery(() => ({
  queryKey: ['credential-protection-rules'],
  queryFn: admin.credentialProtection.rules,
}))
let saving = $state(false)
let saveError = $state('')
const storedEnabled = $derived(settingQuery.data === 'true')
let tab = $state('rules')
let ruleOpen = $state(false)
let detailsOpen = $state(false)
let selectedRule = $state.raw<CredentialRule>()
const rules = $derived(rulesQuery.data?.rules ?? [])
const ruleNames = $derived(new Map(rules.map((rule) => [rule.id, rule.name])))
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
const ruleColumn = createDataTableColumnHelper<CredentialRule>()
const ruleColumns = ruleColumn.columns([
  ruleColumn.accessor((rule) => `${rule.name} ${rule.id} ${rule.target} ${rule.description}`, {
    id: 'rule',
    header: () => m.credential_protection_rule_name(),
    meta: { label: () => m.credential_protection_rule_name() },
    cell: (context) => renderSnippet(ruleNameCell, context),
    size: 380,
  }),
  ruleColumn.accessor((rule) => rule.keywords.join(' '), {
    id: 'conditions',
    header: () => m.credential_protection_rule_conditions(),
    meta: { label: () => m.credential_protection_rule_conditions() },
    cell: (context) => renderSnippet(ruleConditionsCell, context),
    enableSorting: false,
    size: 440,
  }),
])

function openRule(rule: CredentialRule): void {
  selectedRule = rule
  ruleOpen = true
}

let discoveries = $state.raw<CredentialDiscoverySummary[]>([])
let nextCursor = $state<string | null>(null)
let discoveriesLoading = $state(true)
let discoveriesError = $state(false)
let observationGap = $state(false)
let discoveryRetryAppend = false
let testInput = $state('')
let inputElement = $state<HTMLTextAreaElement | null>(null)
let testing = $state(false)
let testError = $state(false)
let matches = $state.raw<CredentialMatch[]>()

onMount(() => {
  void loadDiscoveries(false)
})

async function setEnabled(enabled: boolean): Promise<void> {
  if (!settingQuery.isSuccess || saving || enabled === storedEnabled) return
  const value = enabled ? 'true' : 'false'
  saving = true
  saveError = ''
  try {
    await admin.settings.set(settingKey, value)
    queryClient.setQueryData(queryKey, value)
    toast.success(m.reversible_redaction_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
    toast.error(saveError)
  } finally {
    saving = false
  }
}

async function loadDiscoveries(append: boolean): Promise<void> {
  discoveriesLoading = true
  discoveriesError = false
  discoveryRetryAppend = append
  try {
    const result = await admin.credentialProtection.discoveries({
      cursor: append ? (nextCursor ?? undefined) : undefined,
      limit: 20,
    })
    discoveries = append ? [...discoveries, ...result.items] : result.items
    nextCursor = result.next_cursor
    observationGap = append ? observationGap || result.observation_gap : result.observation_gap
  } catch {
    discoveriesError = true
  } finally {
    discoveriesLoading = false
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

function sourceLabel(source: string): string {
  switch (source) {
    case 'user_message':
      return m.credential_protection_source_user_message()
    case 'system_or_history':
      return m.credential_protection_source_system_or_history()
    case 'tool_arguments':
      return m.credential_protection_source_tool_arguments()
    case 'tool_result':
      return m.credential_protection_source_tool_result()
    case 'other_text':
      return m.credential_protection_source_other_text()
    default:
      return m.credential_protection_source_unknown()
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

{#snippet ruleNameCell(context: DataTableCellContext<CredentialRule>)}
  <button type="button" class="rule-link" onclick={() => openRule(context.row.original)}>
    <span class="font-medium">{context.row.original.name}</span>
  </button>
{/snippet}

{#snippet ruleConditionsCell(context: DataTableCellContext<CredentialRule>)}
  {@const rule = context.row.original}
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

<div class="route-page protection-page">
  <PageHeader
    eyebrow={m.app_shell_nav_advanced_features()}
    title={m.reversible_redaction_title()}
    description={m.credential_protection_workspace_summary()}
    actions={pageActions} />

  <section class="route-section" aria-labelledby="redaction-setting-title">
    <div class="route-section-header">
      <div class="min-w-0 flex-1 basis-64">
        <h2 id="redaction-setting-title" class="route-section-title">{m.reversible_redaction_enable()}</h2>
        <p id="redaction-setting-description" class="route-section-description">
          {m.credential_protection_scope_brief()}
        </p>
        <p id="redaction-setting-immediate" class="route-section-description">{m.common_settings_immediate()}</p>
      </div>
      <div class="flex shrink-0 items-center gap-3">
        {#if saving}<Spinner />{/if}
        <Switch
          id="reversible-redaction-enabled"
          bind:checked={() => storedEnabled, (checked) => void setEnabled(checked)}
          disabled={!settingQuery.isSuccess || saving}
          aria-busy={saving}
          aria-labelledby="redaction-setting-title"
          aria-describedby="redaction-setting-description redaction-setting-immediate" />
      </div>
    </div>
    {#if !settingQuery.isSuccess || saveError}
      <div class="flex flex-col gap-3">
        {#if settingQuery.isPending}
          <p class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
        {:else if settingQuery.isError}
          <Alert.Root variant="destructive"
            ><Alert.Description>{m.reversible_redaction_load_failed()}</Alert.Description></Alert.Root>
          <Button class="self-start" variant="outline" onclick={() => void settingQuery.refetch()}
            >{m.common_retry()}</Button>
        {/if}
        {#if saveError}<Alert.Root variant="destructive"><Alert.Description>{saveError}</Alert.Description></Alert.Root
          >{/if}
      </div>
    {/if}
  </section>

  <Tabs.Root bind:value={tab} class="min-w-0 gap-0">
    <Tabs.List aria-label={m.reversible_redaction_title()}>
      <Tabs.Trigger value="rules"
        ><ListFilterIcon />{m.credential_protection_catalog_title()}
        {#if rulesQuery.isSuccess}<Badge variant="secondary">{formatNumber(rules.length)}</Badge>{/if}
      </Tabs.Trigger>
      <Tabs.Trigger value="records"><HistoryIcon />{m.credential_protection_records_tab()}</Tabs.Trigger>
      <Tabs.Trigger value="test"><ScanTextIcon />{m.credential_protection_test_tab()}</Tabs.Trigger>
    </Tabs.List>

    <Tabs.Content value="rules" class="min-w-0">
      <section class="workspace-panel" aria-labelledby="credential-rules-title">
        <h2 id="credential-rules-title" class="sr-only">{m.credential_protection_catalog_title()}</h2>
        {#if rulesQuery.isError}
          <Alert.Root variant="destructive"
            ><Alert.Description>{m.credential_protection_catalog_failed()}</Alert.Description></Alert.Root>
          <Button class="mt-3" variant="outline" onclick={() => void rulesQuery.refetch()}>{m.common_retry()}</Button>
        {:else}
          <DataTable
            data={rules}
            columns={ruleColumns}
            labels={tableLabels}
            getRowId={(rule) => rule.id}
            ariaLabel={m.credential_protection_catalog_title()}
            size="large"
            stickyHeader
            scrollHeight="clamp(16rem, calc(100svh - 35rem), 42rem)"
            loading={rulesQuery.isPending}
            loadingRows={8}
            globalFilterEnabled
            globalFilterId="credential-rule-search"
            globalFilterPlaceholder={m.credential_protection_catalog_search()}
            paginator
            pagination={{ pageIndex: 0, pageSize: 10 }}
            pageSizeOptions={[10, 25, 50]}
            onRowClick={({ row }) => openRule(row.original)}>
            {#snippet toolbar(table)}
              <p class="text-sm text-muted-foreground" role="status">
                {m.credential_protection_rule_count({ count: formatNumber(table.getFilteredRowModel().rows.length) })}
              </p>
            {/snippet}
            {#snippet empty()}
              <Empty.Root
                ><Empty.Header
                  ><Empty.Media variant="icon"><ListFilterIcon /></Empty.Media>
                  <Empty.Title>{m.credential_protection_catalog_title()}</Empty.Title>
                  <Empty.Description
                    >{rules.length
                      ? m.credential_protection_catalog_no_results()
                      : m.credential_protection_catalog_empty()}</Empty.Description>
                </Empty.Header></Empty.Root>
            {/snippet}
          </DataTable>
        {/if}
      </section>
    </Tabs.Content>

    <Tabs.Content value="records" class="min-w-0">
      <section class="workspace-panel" aria-labelledby="credential-discoveries-title">
        <div class="route-section-header">
          <div>
            <h2 id="credential-discoveries-title" class="sr-only">
              {m.credential_protection_discoveries_title()}
            </h2>
            <p class="route-section-description">{m.credential_protection_records_brief()}</p>
          </div>
          <Button variant="outline" size="sm" disabled={discoveriesLoading} onclick={() => void loadDiscoveries(false)}>
            <RefreshCwIcon data-icon="inline-start" />{m.credential_protection_discoveries_refresh()}
          </Button>
        </div>
        <div class="flex min-w-0 flex-col gap-4">
          {#if observationGap}<Alert.Root
              ><Alert.Description>{m.credential_protection_discoveries_gap()}</Alert.Description></Alert.Root
            >{/if}
          {#if discoveriesError}
            <Alert.Root variant="destructive"
              ><Alert.Description>{m.credential_protection_discoveries_failed()}</Alert.Description></Alert.Root>
            <Button
              class="self-start"
              variant="outline"
              disabled={discoveriesLoading}
              onclick={() => void loadDiscoveries(discoveryRetryAppend)}>{m.common_retry()}</Button>
          {/if}
          {#if discoveriesLoading}<p role="status">{m.credential_protection_discoveries_loading()}</p>{/if}
          {#if !discoveriesLoading && !discoveriesError && !observationGap && discoveries.length === 0}
            <Empty.Root class="min-h-72"
              ><Empty.Header
                ><Empty.Media variant="icon"><HistoryIcon /></Empty.Media>
                <Empty.Title>{m.credential_protection_no_records()}</Empty.Title>
                <Empty.Description>{m.credential_protection_discoveries_empty()}</Empty.Description></Empty.Header>
              <Empty.Content
                ><Button
                  variant="outline"
                  onclick={() => {
                    tab = 'test'
                  }}>{m.credential_protection_test_tab()}<ArrowRightIcon data-icon="inline-end" /></Button
                ></Empty.Content>
            </Empty.Root>
          {/if}
          <Accordion.Root type="multiple">
            {#each discoveries as discovery (discovery.interaction_id)}
              <Accordion.Item value={discovery.interaction_id}>
                <Accordion.Trigger
                  ><span class="discovery-row"
                    ><span class="flex min-w-0 flex-col gap-1">
                      <span>{discovery.api_key_name ?? m.credential_protection_discoveries_unknown_key()}</span>
                      <span class="font-mono text-xs font-normal text-muted-foreground"
                        >{formatLogTime(discovery.discovered_at)}</span>
                    </span>
                    <span class="flex min-w-0 flex-col gap-1">
                      <span
                        >{m.credential_protection_discoveries_count({
                          count: formatNumber(discovery.new_credential_count),
                        })}</span>
                      <span class="truncate text-xs font-normal text-muted-foreground"
                        >{discovery.rule_ids.map((id) => ruleNames.get(id) ?? id).join(' · ')}</span>
                    </span>
                    <Badge variant="outline">{observationStatusLabel(discovery.status)}</Badge>
                  </span></Accordion.Trigger>
                <Accordion.Content>
                  <dl class="grid min-w-0 grid-cols-1 gap-2 text-sm sm:grid-cols-[auto_1fr] sm:gap-x-6">
                    <dt class="text-muted-foreground">{m.credential_protection_discoveries_time()}</dt>
                    <dd>{formatLogTime(discovery.discovered_at)}</dd>
                    <dt class="text-muted-foreground">{m.credential_protection_discoveries_api_key()}</dt>
                    <dd class="break-all">
                      {discovery.api_key_name ?? m.credential_protection_discoveries_unknown_key()}
                    </dd>
                    <dt class="text-muted-foreground">{m.credential_protection_discoveries_rules()}</dt>
                    <dd>
                      <ul>
                        {#each discovery.rule_ids as id (id)}<li class="break-all">
                            {ruleNames.get(id) ?? id} <span class="text-xs text-muted-foreground">({id})</span>
                          </li>{/each}
                      </ul>
                    </dd>
                    <dt class="text-muted-foreground">{m.credential_protection_discoveries_sources()}</dt>
                    <dd>{discovery.source_types.map(sourceLabel).join(' · ')}</dd>
                    <dt class="text-muted-foreground">{m.credential_protection_discoveries_status()}</dt>
                    <dd>{observationStatusLabel(discovery.status)}</dd>
                  </dl>
                  {#if discovery.observation_gap}<Alert.Root class="mt-3"
                      ><Alert.Description>{m.credential_protection_discoveries_gap()}</Alert.Description></Alert.Root
                    >{/if}
                  <Button
                    class="mt-4"
                    variant="outline"
                    href={resolve(`/logs?interaction=${encodeURIComponent(discovery.interaction_id)}`)}
                    >{m.credential_protection_discoveries_open()}</Button>
                </Accordion.Content>
              </Accordion.Item>
            {/each}
          </Accordion.Root>
          {#if nextCursor}<Button
              class="self-start"
              variant="outline"
              disabled={discoveriesLoading}
              onclick={() => void loadDiscoveries(true)}>{m.credential_protection_discoveries_more()}</Button
            >{/if}
        </div>
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
                    rows={12}
                    class="min-h-72 max-h-[36rem] resize-y font-mono"
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
          <div class="test-results-pane" aria-labelledby="test-results-title" aria-busy={testing}>
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
                  <li>
                    <Button
                      variant="outline"
                      class="h-auto w-full justify-start whitespace-normal py-3 text-left"
                      onclick={() => selectMatch(match)}
                      ><span class="flex min-w-0 flex-col gap-1"
                        ><span class="break-all"
                          >{ruleNames.get(match.rule_id) ?? match.rule_id}
                          <span class="text-xs text-muted-foreground">({match.rule_id})</span></span
                        ><span
                          >{m.credential_protection_tester_position({
                            startLine: match.start_line,
                            startColumn: match.start_column,
                            endLine: match.end_line,
                            endColumn: match.end_column,
                          })}</span
                        ></span
                      ></Button>
                  </li>
                {/each}
              </ul>
            {/if}
          </div>
        </div>
      </section>
    </Tabs.Content>
  </Tabs.Root>
</div>

<Sheet.Root bind:open={ruleOpen}>
  <Sheet.Content
    closeLabel={m.common_close()}
    class="data-[side=right]:w-full data-[side=right]:sm:max-w-xl overflow-y-auto">
    <Sheet.Header class="pr-12">
      <Sheet.Title>{selectedRule?.name ?? m.credential_protection_catalog_title()}</Sheet.Title>
      <Sheet.Description>{selectedRule?.description}</Sheet.Description>
    </Sheet.Header>
    {#if selectedRule}
      <div class="flex min-w-0 flex-col gap-5 px-4 pb-6">
        <section aria-labelledby="rule-keywords-title">
          <h3 id="rule-keywords-title" class="mb-2 text-sm font-medium">{m.credential_protection_keywords()}</h3>
          {#if selectedRule.keywords.length}
            <div class="flex flex-wrap gap-1.5">
              {#each selectedRule.keywords as keyword (keyword)}
                <Badge variant="secondary" class="h-auto max-w-full"
                  ><code class="whitespace-normal break-all">{keyword}</code></Badge>
              {/each}
            </div>
            <p class="mt-2 text-xs text-muted-foreground">
              {selectedRule.skip_report
                ? m.credential_protection_component_keywords_help()
                : m.credential_protection_keywords_help()}
            </p>
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
            <h3 id="rule-exclusions-title" class="mb-2 text-sm font-medium">{m.credential_protection_exclusions()}</h3>
            <p class="mb-2 text-xs text-muted-foreground">{m.credential_protection_exclusions_help()}</p>
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
                <dt>{m.credential_protection_confidence()}</dt>
                <dd>{confidenceLabel(selectedRule.confidence)}</dd>
                <dt>{m.credential_protection_rule_usage()}</dt>
                <dd>
                  {selectedRule.skip_report
                    ? m.credential_protection_component_only()
                    : m.credential_protection_standalone()}
                </dd>
              </dl>
              <p class="mt-3 text-xs text-muted-foreground">{m.credential_protection_confidence_help()}</p>
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
.protection-page {
  max-width: 90rem;
  margin-inline: auto;
  width: 100%;
}
.workspace-panel {
  min-width: 0;
  padding-top: 1.5rem;
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
  font-size: 0.875rem;
}
.rule-parameters dt {
  color: var(--muted-foreground);
}
.rule-parameters dd {
  text-align: end;
}
.discovery-row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1.2fr) auto;
  align-items: center;
  gap: 1.5rem;
  width: 100%;
  text-align: start;
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
  border: 1px solid var(--border);
  border-radius: var(--radius);
  overflow: hidden;
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
  .discovery-row {
    grid-template-columns: minmax(0, 1fr);
    gap: 0.5rem;
  }
  .test-workspace {
    grid-template-columns: minmax(0, 1fr);
  }
  .test-results-pane {
    border-left: 0;
    border-top: 1px solid var(--border);
  }
}
</style>
