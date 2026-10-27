<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { renderSnippet } from '@tanstack/svelte-table'
import { formatDuration, formatLogTime } from '#lib/format.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import { clientErrorTypeLabel, clientErrorTypeTone, failureOriginLabel } from '#lib/observation-labels.js'
import type { ClientErrorSummary } from '#lib/types.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableRow,
  type DataTableRowPointerEvent,
} from '#lib/components/ui/data-table/index.js'

let {
  items,
  loading,
  onselect,
}: { items: ClientErrorSummary[]; loading: boolean; onselect: (request: ClientErrorSummary) => void } = $props()
const labels = $derived(getDataTableLabels())
const helper = createDataTableColumnHelper<ClientErrorSummary>()

function sourceLabel(row: ClientErrorSummary): string {
  return (
    row.api_key_name ??
    row.client ??
    (row.kind === 'rejection' && !row.observation_gap ? m.failed_request_unauthenticated() : '—')
  )
}
const modelLabel = (row: ClientErrorSummary): string => row.model_display_name ?? row.model ?? '—'
const servicesLabel = (row: ClientErrorSummary): string => row.services.map((service) => service.name).join(', ') || '—'
const originLabel = (row: ClientErrorSummary): string => failureOriginLabel(row.error.source) ?? '—'
const errorLabel = (row: ClientErrorSummary): string => row.error.message ?? row.error.code ?? '—'
const durationLabel = (row: ClientErrorSummary): string =>
  row.duration_ms == null ? '—' : formatDuration(Number(row.duration_ms))

// 时间、类型、来源与耗时保持紧凑；多出的宽度只交给错误列，模型与供应商不抢占。
const columns = helper.columns([
  helper.accessor('started_at', {
    header: () => m.failed_request_time(),
    cell: ({ row }) => renderSnippet(requestTime, row.original),
    size: 149,
    enableSorting: false,
    meta: { width: 'compact', cellClass: 'font-technical text-xs tabular-nums' },
  }),
  helper.accessor('type', {
    header: () => m.observation_client_error_type(),
    cell: ({ row }) => renderSnippet(typeCell, row.original),
    size: 104,
    enableSorting: false,
    meta: { width: 'compact' },
  }),
  helper.accessor(sourceLabel, {
    id: 'source',
    header: () => m.failed_request_client(),
    size: 118,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, sourceLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(modelLabel, {
    id: 'model',
    header: () => m.common_model(),
    size: 90,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, modelLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(servicesLabel, {
    id: 'services',
    header: () => m.common_model_service(),
    size: 90,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, servicesLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(originLabel, {
    id: 'origin',
    header: () => m.failed_request_origin(),
    size: 70,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, originLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(errorLabel, {
    id: 'error',
    header: () => m.failed_request_error(),
    size: 160,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(errorCell, row.original),
    meta: { width: 'fill', cellClass: 'max-w-0' },
  }),
  helper.accessor('duration_ms', {
    header: () => m.failed_request_duration(),
    size: 71,
    enableSorting: false,
    cell: ({ row }) => durationLabel(row.original),
    meta: { width: 'compact', align: 'end', cellClass: 'font-technical text-xs tabular-nums' },
  }),
])
</script>

{#snippet clippedCell(value: string)}
  <span class="block truncate" title={value}>{value}</span>
{/snippet}

{#snippet typeCell(request: ClientErrorSummary)}
  <Badge variant={clientErrorTypeTone(request.type)}>{clientErrorTypeLabel(request.type)}</Badge>
{/snippet}

{#snippet requestTime(request: ClientErrorSummary)}
  <button
    class="text-start underline-offset-4 hover:underline"
    onclick={(event) => {
      event.stopPropagation()
      onselect(request)
    }}>
    {formatLogTime(request.started_at)}
  </button>
{/snippet}

{#snippet errorCell(request: ClientErrorSummary)}
  <div class="min-w-0">
    <span class="block truncate" title={request.error.message ?? request.error.code ?? undefined}>
      {errorLabel(request)}
    </span>
    {#if request.error.status_code !== null}
      <span class="font-technical text-xs text-muted-foreground">HTTP {request.error.status_code}</span>
    {/if}
  </div>
{/snippet}

<div class="client-error-table">
  <div class="desktop-table">
    <DataTable
      data={items}
      {columns}
      {labels}
      {loading}
      ariaLabel={m.observation_client_errors()}
      getRowId={(row: ClientErrorSummary) => `${row.kind}:${row.id}`}
      class="isolate flex-1"
      workbench
      stickyHeader
      selectionMode="single"
      selectOnRowClick={false}
      onRowSelect={(row: DataTableRow<ClientErrorSummary>) => onselect(row.original)}
      onRowClick={({ row }: DataTableRowPointerEvent<ClientErrorSummary>) => onselect(row.original)} />
  </div>
  <!-- 窄工作区：先模型、类型、实际错误与时间，再以 dl 列出客户端／密钥、供应商、来源与耗时。 -->
  <ul class="mobile-list" aria-label={m.observation_client_errors()} aria-busy={loading || undefined}>
    {#each items as request (`${request.kind}:${request.id}`)}
      <!-- 行本身只响应指针；键盘用户通过行内时间按钮打开详情，避免在按钮内嵌套按钮。 -->
      <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
      <li class="failure-row" onclick={() => onselect(request)}>
        <div class="failure-head">
          <p class="failure-model">{modelLabel(request)}</p>
          <div class="failure-head-meta">
            {@render typeCell(request)}
            {@render requestTime(request)}
          </div>
        </div>
        <p class="failure-error">{errorLabel(request)}</p>
        {#if request.error.status_code !== null}
          <p class="font-technical text-xs text-muted-foreground">HTTP {request.error.status_code}</p>
        {/if}
        <dl class="failure-facts">
          <dt>{m.failed_request_client()}</dt>
          <dd>{sourceLabel(request)}</dd>
          <dt>{m.common_model_service()}</dt>
          <dd>{servicesLabel(request)}</dd>
          <dt>{m.failed_request_origin()}</dt>
          <dd>{originLabel(request)}</dd>
          <dt>{m.failed_request_duration()}</dt>
          <dd class="font-technical tabular-nums">{durationLabel(request)}</dd>
        </dl>
      </li>
    {/each}
  </ul>
</div>

<style>
.client-error-table {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
  overflow: hidden;
}
.desktop-table {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
}
.mobile-list {
  display: none;
  min-height: 0;
  flex: 1;
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-y: auto;
}
.failure-row {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 0.35rem;
  border-bottom: 1px solid var(--border);
  padding: 0.85rem 0.75rem;
  cursor: pointer;
}
.failure-row:hover,
.failure-row:focus-visible {
  background: color-mix(in oklab, var(--muted) 45%, var(--background));
}
.failure-row:focus-visible {
  outline: 2px solid var(--ring);
  outline-offset: -2px;
}
.failure-head {
  display: flex;
  min-width: 0;
  flex-wrap: wrap;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.25rem 0.75rem;
}
.failure-head-meta {
  display: flex;
  flex: 1 1 auto;
  min-width: 0;
  flex-wrap: wrap;
  align-items: center;
  justify-content: flex-end;
  gap: 0.4rem;
  margin-inline-start: auto;
}
.failure-head :global(button) {
  flex: none;
  font-family: var(--font-technical);
  font-size: 0.75rem;
  font-variant-numeric: tabular-nums;
  color: var(--muted-foreground);
}
.failure-model {
  min-width: 0;
  font-weight: 500;
  overflow-wrap: anywhere;
}
.failure-error {
  font-size: 0.8125rem;
  overflow-wrap: anywhere;
}
.failure-facts {
  display: grid;
  grid-template-columns: max-content minmax(0, 1fr);
  gap: 0.15rem 0.75rem;
  margin: 0;
  font-size: 0.75rem;
}
.failure-facts dt {
  color: var(--muted-foreground);
}
.failure-facts dd {
  min-width: 0;
  overflow-wrap: anywhere;
}
/* 按所在工作区宽度切换，与 route-mobile-list 的断点一致。 */
@container route-page (max-width: 52rem) {
  .desktop-table {
    display: none;
  }
  .mobile-list {
    display: block;
  }
}
</style>
