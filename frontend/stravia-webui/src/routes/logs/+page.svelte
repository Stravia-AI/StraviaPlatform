<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import SlidersHorizontalIcon from '@lucide/svelte/icons/sliders-horizontal'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { getDataTableLabels } from '$lib/data-table-labels'
import {
  computeTps,
  formatDuration,
  formatDurationSeconds,
  formatLogTime,
  formatTokenCount,
  formatTps,
} from '$lib/format'
import type { RequestLog } from '$lib/types'
import LogDetailDialog from '$lib/components/log-detail-dialog.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import TechnicalValue from '$lib/components/technical-value.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRow,
} from '$lib/components/ui/data-table'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Skeleton } from '$lib/components/ui/skeleton'

const pageSize = 25
type RequestFilterKey = 'provider' | 'model' | 'apiKey' | 'status'

interface ActiveRequestFilter {
  key: RequestFilterKey
  label: string
  value: string
}

const queryClient = useQueryClient()
let pageIndex = $state(0)
let providerFilter = $state('all')
let modelFilter = $state('all')
let apiKeyFilter = $state('all')
let statusFilter = $state('all')
let filterOpen = $state(false)
let clearOpen = $state(false)
let clearing = $state(false)
let selectedLog = $state<RequestLog>()
let detailOpen = $state(false)

const providersQuery = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const logsQuery = createQuery(() => ({
  queryKey: ['logs', pageIndex, providerFilter, modelFilter, apiKeyFilter, statusFilter],
  queryFn: () =>
    admin.logs.query({
      limit: pageSize,
      offset: pageIndex * pageSize,
      provider: providerFilter === 'all' ? undefined : providerFilter,
      model: modelFilter === 'all' ? undefined : modelFilter,
      api_key: apiKeyFilter === 'all' ? undefined : apiKeyFilter,
      status_min: statusFilter === 'success' ? 200 : statusFilter === 'error' ? 400 : undefined,
      status_max: statusFilter === 'success' ? 399 : statusFilter === 'error' ? 599 : undefined,
    }),
  refetchInterval: 5_000,
}))

const logs = $derived(logsQuery.data?.items ?? [])
const total = $derived(logsQuery.data?.total ?? 0)
const pageCount = $derived(Math.max(1, Math.ceil(total / pageSize)))
const tableLabels = $derived(getDataTableLabels())
const modelLabel = (log: RequestLog) =>
  log.model_name ??
  modelsQuery.data?.find((model) => model.id === log.model_id)?.name ??
  log.model_id ??
  log.client_model ??
  '–'
const providerLabel = (log: RequestLog) =>
  log.provider_name ??
  providersQuery.data?.find((provider) => provider.id === log.provider_id)?.name ??
  log.provider_id ??
  '–'
const upstreamModelLabel = (log: RequestLog) => log.upstream_model ?? '–'
const logColumnHelper = createDataTableColumnHelper<RequestLog>()
const logColumns = logColumnHelper.columns([
  logColumnHelper.accessor('created_at', {
    header: () => m.logs_time(),
    cell: (context) => formatLogTime(context.getValue()),
    enableSorting: false,
    meta: { label: () => m.logs_time(), cellClass: 'font-technical whitespace-nowrap text-xs tabular-nums' },
  }),
  logColumnHelper.accessor((log) => log.path ?? '–', {
    id: 'request',
    header: () => m.logs_request(),
    cell: (context) => renderSnippet(logRequestCell, context),
    enableSorting: false,
    meta: { label: () => m.logs_request() },
  }),
  logColumnHelper.accessor((log) => modelLabel(log), {
    id: 'logicalModel',
    header: () => m.logs_logical_model(),
    cell: (context) => renderSnippet(logicalModelCell, context),
    enableSorting: false,
    meta: { label: () => m.logs_logical_model() },
  }),
  logColumnHelper.accessor((log) => upstreamModelLabel(log), {
    id: 'upstreamModel',
    header: () => m.logs_upstream_model(),
    cell: (context) => renderSnippet(upstreamModelCell, context),
    enableSorting: false,
    meta: { label: () => m.logs_upstream_model() },
  }),
  logColumnHelper.accessor('thinking_level', {
    header: () => m.logs_thinking_level(),
    cell: (context) => context.getValue() ?? '–',
    enableSorting: false,
    meta: {
      label: () => m.logs_thinking_level(),
      cellClass: 'font-technical whitespace-nowrap text-xs text-muted-foreground',
    },
    size: 88,
  }),
  logColumnHelper.accessor('client_status_code', {
    header: () => m.common_status(),
    cell: (context) => renderSnippet(logStatusCell, context),
    enableSorting: false,
    meta: { label: () => m.common_status() },
  }),
  logColumnHelper.accessor(
    (log) =>
      `${m.logs_first_token_short()}: ${formatDurationSeconds(log.stream_first_chunk_ms)} / ${m.logs_duration_short()}: ${formatDurationSeconds(log.latency_total_ms)}`,
    {
      id: 'latency',
      header: () => m.common_latency(),
      cell: (context) => renderSnippet(latencyCell, context),
      enableSorting: false,
      meta: {
        label: () => m.common_latency(),
        align: 'end',
        cellClass: 'whitespace-nowrap',
      },
      size: 152,
    },
  ),
  logColumnHelper.accessor((log) => computeTps(log), {
    id: 'tokenSpeed',
    header: () => m.logs_token_speed(),
    cell: (context) => formatTps(context.getValue()),
    enableSorting: false,
    meta: {
      label: () => m.logs_token_speed(),
      align: 'end',
      cellClass: 'font-technical whitespace-nowrap text-xs tabular-nums',
    },
  }),
  logColumnHelper.accessor((log) => log.input_tokens + log.output_tokens, {
    id: 'tokens',
    header: () => m.common_token(),
    cell: (context) => renderSnippet(tokenUsageCell, context),
    enableSorting: false,
    meta: {
      label: () => m.common_token(),
      cellClass: 'whitespace-nowrap',
    },
    size: 156,
  }),
  logColumnHelper.display({
    id: 'actions',
    header: () => m.logs_action(),
    cell: (context) => renderSnippet(logActionCell, context),
    enableHiding: false,
    enableSorting: false,
    meta: { label: () => m.logs_action(), align: 'end', exportable: false },
    size: 88,
  }),
])

function getLogRowId(log: RequestLog): string {
  return log.id
}

function logRowClass(row: DataTableRow<RequestLog>): string | undefined {
  return selectedLog?.id === row.original.id ? 'bg-muted' : undefined
}
const activeFilters = $derived.by(() => {
  const filters: ActiveRequestFilter[] = []
  if (providerFilter !== 'all') {
    filters.push({
      key: 'provider',
      label: m.common_model_service(),
      value: providersQuery.data?.find((provider) => provider.id === providerFilter)?.name ?? providerFilter,
    })
  }
  if (modelFilter !== 'all') {
    filters.push({
      key: 'model',
      label: m.common_model(),
      value: modelsQuery.data?.find((model) => model.id === modelFilter)?.name ?? modelFilter,
    })
  }
  if (apiKeyFilter !== 'all') {
    filters.push({
      key: 'apiKey',
      label: m.common_api_key(),
      value: keysQuery.data?.find((key) => key.id === apiKeyFilter)?.name ?? apiKeyFilter,
    })
  }
  if (statusFilter !== 'all') {
    filters.push({
      key: 'status',
      label: m.common_status(),
      value: statusFilter === 'success' ? m.logs_success() : m.common_errors_label(),
    })
  }
  return filters
})
const activeFilterCount = $derived(activeFilters.length)

function selectLog(log: RequestLog): void {
  selectedLog = log
  detailOpen = true
}

function resetPage(): void {
  pageIndex = 0
}

function clearFilters(): void {
  providerFilter = 'all'
  modelFilter = 'all'
  apiKeyFilter = 'all'
  statusFilter = 'all'
  pageIndex = 0
}

function removeFilter(key: RequestFilterKey): void {
  if (key === 'provider') providerFilter = 'all'
  else if (key === 'model') modelFilter = 'all'
  else if (key === 'apiKey') apiKeyFilter = 'all'
  else statusFilter = 'all'
  pageIndex = 0
}

async function clearRequests(): Promise<void> {
  clearing = true
  try {
    await admin.logs.clear()
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['logs'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-overview'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-hourly'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-models'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-providers'] }),
      queryClient.invalidateQueries({ queryKey: ['stats-api-keys'] }),
    ])
    clearOpen = false
    pageIndex = 0
    toast.success(m.logs_request_history_cleared())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    clearing = false
  }
}
</script>

{#snippet logRequestCell(context: DataTableCellContext<RequestLog>)}
  {@const log = context.row.original}
  <div class="flex min-w-0 items-center gap-2">
    <Badge variant="outline">{log.method ?? '–'}</Badge>
    <TechnicalValue value={log.path ?? '–'} copyable />
  </div>
{/snippet}

{#snippet logicalModelCell(context: DataTableCellContext<RequestLog>)}
  <p class="max-w-72 truncate font-medium">{modelLabel(context.row.original)}</p>
{/snippet}

{#snippet upstreamModelCell(context: DataTableCellContext<RequestLog>)}
  <div class="max-w-72">
    <p class="truncate font-medium">{upstreamModelLabel(context.row.original)}</p>
    <p class="truncate text-xs text-muted-foreground">{providerLabel(context.row.original)}</p>
  </div>
{/snippet}

{#snippet logStatusCell(context: DataTableCellContext<RequestLog>)}
  {@const status = context.row.original.client_status_code}
  <StatusIndicator
    compact
    label={String(status ?? '–')}
    tone={status == null ? 'neutral' : status >= 400 ? 'error' : 'healthy'} />
{/snippet}

{#snippet latencyCell(context: DataTableCellContext<RequestLog>)}
  {@const log = context.row.original}
  <div class="font-technical grid w-max grid-cols-[auto_auto] justify-end gap-x-2 text-[11px] leading-4 tabular-nums">
    <span class="text-muted-foreground">{m.logs_first_token_short()}</span>
    <span>{formatDurationSeconds(log.stream_first_chunk_ms)}</span>
    <span class="text-muted-foreground">{m.logs_duration_short()}</span>
    <span>{formatDurationSeconds(log.latency_total_ms)}</span>
  </div>
{/snippet}

{#snippet tokenUsage(log: RequestLog)}
  <span title={m.logs_input_tokens()}><span class="text-muted-foreground">{m.common_input_abbreviation()}</span>
    {formatTokenCount(log.input_tokens)}</span>
  <span title={m.logs_output_tokens()}><span class="text-muted-foreground">{m.common_output_abbreviation()}</span>
    {formatTokenCount(log.output_tokens)}</span>
  <span title={m.logs_cache_input_tokens()}><span class="text-muted-foreground">C-IN</span>
    {formatTokenCount(log.cache_read_tokens)}</span>
  <span title={m.logs_cache_output_tokens()}><span class="text-muted-foreground">C-OUT</span>
    {formatTokenCount(log.cache_write_tokens)}</span>
{/snippet}

{#snippet tokenUsageCell(context: DataTableCellContext<RequestLog>)}
  <div class="font-technical grid w-max grid-cols-2 gap-x-3 gap-y-0.5 text-[11px] leading-4 tabular-nums">
    {@render tokenUsage(context.row.original)}
  </div>
{/snippet}

{#snippet logActionCell(context: DataTableCellContext<RequestLog>)}
  <Button variant="ghost" size="sm" onclick={() => selectLog(context.row.original)}>
    {m.logs_view_details()}
  </Button>
{/snippet}

<svelte:head><title>{m.common_request_history()} · Stravia</title></svelte:head>

{#snippet liveMeta()}
  <StatusIndicator
    compact
    label={logsQuery.isFetching ? m.logs_updating_5s() : m.logs_live_5s()}
    tone={logsQuery.isError ? 'error' : 'healthy'} />
{/snippet}

{#snippet clearAction()}
  <Button variant="destructive" onclick={() => (clearOpen = true)} disabled={total === 0}
    >{m.logs_clear_history()}</Button>
{/snippet}

{#snippet filters()}
  <Field.Field>
    <Field.FieldLabel for="request-provider-filter">{m.common_model_service()}</Field.FieldLabel>
    <Select.Root type="single" bind:value={providerFilter}>
      <Select.Trigger id="request-provider-filter" class="w-full"
        >{providersQuery.data?.find((provider) => provider.id === providerFilter)?.name ??
          m.logs_all_model_services()}</Select.Trigger>
      <Select.Content
        ><Select.Group
          ><Select.Item value="all" onclick={resetPage}>{m.logs_all_model_services()}</Select.Item
          >{#each providersQuery.data ?? [] as provider (provider.id)}<Select.Item
              value={provider.id}
              label={provider.name}
              onclick={resetPage}>{provider.name}</Select.Item
            >{/each}</Select.Group
        ></Select.Content>
    </Select.Root>
  </Field.Field>
  <Field.Field>
    <Field.FieldLabel for="request-model-filter">{m.common_model()}</Field.FieldLabel>
    <Select.Root type="single" bind:value={modelFilter}>
      <Select.Trigger id="request-model-filter" class="w-full"
        >{modelsQuery.data?.find((model) => model.id === modelFilter)?.name ?? m.common_all_models()}</Select.Trigger>
      <Select.Content
        ><Select.Group
          ><Select.Item value="all" onclick={resetPage}>{m.common_all_models()}</Select.Item
          >{#each modelsQuery.data ?? [] as model (model.id)}<Select.Item
              value={model.id}
              label={model.name}
              onclick={resetPage}>{model.name}</Select.Item
            >{/each}</Select.Group
        ></Select.Content>
    </Select.Root>
  </Field.Field>
  <Field.Field>
    <Field.FieldLabel for="request-key-filter">{m.common_api_key()}</Field.FieldLabel>
    <Select.Root type="single" bind:value={apiKeyFilter}>
      <Select.Trigger id="request-key-filter" class="w-full"
        >{keysQuery.data?.find((key) => key.id === apiKeyFilter)?.name ?? m.logs_all_api_keys()}</Select.Trigger>
      <Select.Content
        ><Select.Group
          ><Select.Item value="all" onclick={resetPage}>{m.logs_all_api_keys()}</Select.Item
          >{#each keysQuery.data ?? [] as key (key.id)}<Select.Item value={key.id} label={key.name} onclick={resetPage}
              >{key.name}</Select.Item
            >{/each}</Select.Group
        ></Select.Content>
    </Select.Root>
  </Field.Field>
  <Field.Field>
    <Field.FieldLabel for="request-status-filter">{m.common_status()}</Field.FieldLabel>
    <Select.Root type="single" bind:value={statusFilter}>
      <Select.Trigger id="request-status-filter" class="w-full"
        >{statusFilter === 'success'
          ? m.logs_success()
          : statusFilter === 'error'
            ? m.common_errors_label()
            : m.logs_all_statuses()}</Select.Trigger>
      <Select.Content
        ><Select.Group
          ><Select.Item value="all" onclick={resetPage}>{m.logs_all_statuses()}</Select.Item><Select.Item
            value="success"
            onclick={resetPage}>{m.logs_success_2xx_3xx()}</Select.Item
          ><Select.Item value="error" onclick={resetPage}>{m.logs_errors_4xx_5xx()}</Select.Item></Select.Group
        ></Select.Content>
    </Select.Root>
  </Field.Field>
{/snippet}

{#snippet activeFilterRows()}
  {#if activeFilters.length > 0}
    <div class="divide-y border-y" aria-label={m.logs_active_request_filters()}>
      {#each activeFilters as filter (filter.key)}
        <div class="flex min-h-10 items-center justify-between gap-3 py-1 text-sm">
          <p class="min-w-0 truncate">
            <span class="text-muted-foreground">{filter.label}:</span>
            <span class="font-medium">{filter.value}</span>
          </p>
          <Button
            variant="ghost"
            size="sm"
            aria-label={m.logs_remove_value_filter({ label: filter.label })}
            onclick={() => removeFilter(filter.key)}>{m.common_remove()}</Button>
        </div>
      {/each}
    </div>
  {/if}
{/snippet}

{#snippet desktopActiveFilters()}
  {#each activeFilters as filter (filter.key)}
    <Button
      variant="outline"
      size="sm"
      aria-label={m.logs_remove_value_filter({ label: filter.label })}
      onclick={() => removeFilter(filter.key)}>
      <span class="text-muted-foreground">{filter.label}:</span>
      <span class="max-w-40 truncate">{filter.value}</span>
      <span aria-hidden="true">×</span>
    </Button>
  {/each}
  {#if activeFilterCount > 1}
    <Button variant="ghost" size="sm" onclick={clearFilters}>{m.logs_clear_filters()}</Button>
  {/if}
{/snippet}

{#snippet desktopFilterAction()}
  <Button variant="outline" size="sm" onclick={() => (filterOpen = true)}>
    <SlidersHorizontalIcon data-icon="inline-start" />
    {m.logs_filters()}
    {#if activeFilterCount > 0}
      <span class="font-technical tabular-nums">· {activeFilterCount}</span>
    {/if}
  </Button>
{/snippet}

{#snippet tableEmpty()}
  <Empty.Root class="py-8">
    <Empty.Header>
      <Empty.Title>
        {activeFilterCount > 0 ? m.logs_no_requests_match_filters() : m.logs_no_requests_recorded()}
      </Empty.Title>
      <Empty.Description>
        {activeFilterCount > 0
          ? m.logs_remove_filter_wait_matching_request()
          : m.logs_requests_appear_app_sends_traffic_stravia()}
      </Empty.Description>
    </Empty.Header>
    <Empty.Content>
      {#if activeFilterCount > 0}
        <Button variant="outline" onclick={clearFilters}>{m.logs_clear_filters()}</Button>
      {:else}
        <Button href="/connect">{m.connect_connect_apps()}</Button>
      {/if}
    </Empty.Content>
  </Empty.Root>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.common_monitor()}
    title={m.common_request_history()}
    description={m.logs_page_summary()}
    meta={liveMeta}
    actions={clearAction} />

  <section class="route-section" aria-labelledby="request-ledger-title">
    <div class="route-section-header">
      <div>
        <h2 id="request-ledger-title" class="route-section-title">{m.logs_recent_requests()}</h2>
        <p class="route-section-description">
          {m.logs_sensitive_content_notice()}
        </p>
      </div>
      {#if total > 0}
        <span class="font-technical text-xs text-muted-foreground tabular-nums">{total}</span>
      {/if}
    </div>

    {#if logs.length > 0 || activeFilterCount > 0}
      <div class="mb-4 flex items-center justify-between md:hidden">
        <Button variant="outline" onclick={() => (filterOpen = true)}
          ><SlidersHorizontalIcon data-icon="inline-start" />{m.logs_filters()}{#if activeFilterCount > 0}<span
              class="font-technical">· {activeFilterCount}</span
            >{/if}</Button>
        {#if activeFilterCount > 0}<Button variant="ghost" size="sm" onclick={clearFilters}>{m.logs_clear()}</Button
          >{/if}
      </div>
    {/if}

    {#if logsQuery.isPending}
      <div class="flex flex-col border-y" aria-label={m.logs_loading_requests()}>
        {#each Array(8) as _, index (index)}<div
            class="grid grid-cols-[1fr_3fr_2fr_1fr] gap-4 border-b p-3 last:border-b-0">
            <Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" />
          </div>{/each}
      </div>
    {:else if logsQuery.isError}
      <div class="border-y py-6">
        <p class="text-sm font-medium text-destructive">
          {m.logs_requests_not_loaded()}
        </p>
        <p class="mt-1 text-sm text-muted-foreground">
          {localizeBackendErrorMessage(logsQuery.error)}
        </p>
        <Button class="mt-3" variant="outline" onclick={() => void logsQuery.refetch()}>{m.common_retry()}</Button>
      </div>
    {:else}
      <div class="route-desktop-table">
        <DataTable
          data={logs}
          columns={logColumns}
          labels={tableLabels}
          getRowId={getLogRowId}
          columnVisibility={{ request: false }}
          ariaLabel={m.logs_recent_requests()}
          rowClass={logRowClass}
          stripedRows
          toolbar={desktopActiveFilters}
          toolbarEnd={desktopFilterAction}
          columnToggle
          exportable
          exportFilename="request-history.csv"
          empty={tableEmpty} />
      </div>

      <div class="route-mobile-list">
        {#if logs.length === 0}
          <div class="border-y">{@render tableEmpty()}</div>
        {:else}
          {#each logs as log (log.id)}
            <div class="route-mobile-row">
              <div class="min-w-0">
                <div class="flex min-w-0 items-center gap-2">
                  <Badge variant="outline">{log.method ?? '–'}</Badge><TechnicalValue
                    value={log.path ?? '–'}
                    copyable />
                </div>
                <dl class="mt-2 grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 text-sm">
                  <dt class="text-muted-foreground">{m.logs_logical_model()}</dt>
                  <dd class="truncate font-medium">{modelLabel(log)}</dd>
                  <dt class="text-muted-foreground">{m.logs_upstream_model()}</dt>
                  <dd class="truncate font-medium">
                    {upstreamModelLabel(log)}
                    <span class="font-normal text-muted-foreground">· {providerLabel(log)}</span>
                  </dd>
                </dl>
                <div class="mt-1 flex flex-wrap items-center gap-x-3">
                  <StatusIndicator
                    compact
                    label={String(log.client_status_code ?? '–')}
                    tone={log.client_status_code == null
                      ? 'neutral'
                      : log.client_status_code >= 400
                        ? 'error'
                        : 'healthy'} /><span class="font-technical text-xs text-muted-foreground"
                    >{formatDuration(log.latency_total_ms)}</span
                  >{#if log.thinking_level}<span class="font-technical text-xs text-muted-foreground"
                      >{m.logs_thinking_level()} {log.thinking_level}</span
                    >{/if}
                </div>
                <div class="font-technical mt-1 grid grid-cols-2 gap-x-3 text-[11px] leading-4 tabular-nums">
                  {@render tokenUsage(log)}
                </div>
                <time class="font-technical mt-1 block text-xs text-muted-foreground"
                  >{formatLogTime(log.created_at)}</time>
              </div>
              <Button variant="ghost" size="sm" onclick={() => selectLog(log)}>{m.logs_view_details()}</Button>
            </div>
          {/each}
        {/if}
      </div>
    {/if}

    {#if total > 0}
      <div class="mt-4 flex items-center justify-between gap-4 border-t pt-4">
        <p class="font-technical text-xs text-muted-foreground tabular-nums">
          {m.logs_pagination({ pageIndex: pageIndex + 1, pageCount: pageCount })}
        </p>
        <div class="flex gap-2">
          <Button variant="outline" size="sm" disabled={pageIndex === 0} onclick={() => (pageIndex -= 1)}
            >{m.logs_previous()}</Button
          ><Button variant="outline" size="sm" disabled={pageIndex + 1 >= pageCount} onclick={() => (pageIndex += 1)}
            >{m.logs_next()}</Button>
        </div>
      </div>
    {/if}
  </section>
</div>

<Sheet.Root bind:open={filterOpen}>
  <Sheet.Content
    side="right"
    class="w-full! max-w-none! gap-0 p-0 sm:max-w-sm!"
    closeLabel={m.logs_close_request_filters()}>
    <Sheet.Header class="border-b"
      ><Sheet.Title>{m.logs_request_filters()}</Sheet.Title><Sheet.Description
        >{m.logs_choose_filters_show_only_requests_need()}</Sheet.Description
      ></Sheet.Header>
    <div class="route-overlay-body flex flex-col gap-5">
      {@render activeFilterRows()}
      <Field.FieldGroup>{@render filters()}</Field.FieldGroup>
    </div>
    <Sheet.Footer class="route-overlay-footer"
      ><Button variant="outline" onclick={clearFilters}>{m.logs_clear_filters()}</Button><Sheet.Close
        class="h-10 rounded-md bg-primary px-3 text-primary-foreground">{m.logs_show_requests()}</Sheet.Close
      ></Sheet.Footer>
  </Sheet.Content>
</Sheet.Root>

<LogDetailDialog bind:open={detailOpen} logId={selectedLog?.id} summary={selectedLog} />

<AlertDialog.Root bind:open={clearOpen}>
  <AlertDialog.Content
    ><AlertDialog.Header
      ><AlertDialog.Title>{m.logs_clear_all_request_history()}</AlertDialog.Title><AlertDialog.Description
        >{m.logs_clear_history_warning()}</AlertDialog.Description
      ></AlertDialog.Header
    ><AlertDialog.Footer
      ><AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel><AlertDialog.Action
        variant="destructive"
        disabled={clearing}
        onclick={() => void clearRequests()}
        >{clearing ? m.logs_clearing() : m.logs_clear_all_history()}</AlertDialog.Action
      ></AlertDialog.Footer
    ></AlertDialog.Content>
</AlertDialog.Root>
