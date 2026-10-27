<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { renderSnippet } from '@tanstack/svelte-table'
import { formatDuration, formatLogTime } from '#lib/format.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import { failureOriginLabel, upstreamErrorOutcomeLabel, upstreamErrorOutcomeTone } from '#lib/observation-labels.js'
import type { UpstreamErrorSummary } from '#lib/types.js'
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
}: { items: UpstreamErrorSummary[]; loading: boolean; onselect: (error: UpstreamErrorSummary) => void } = $props()
const labels = $derived(getDataTableLabels())
const helper = createDataTableColumnHelper<UpstreamErrorSummary>()

const clientLabel = (row: UpstreamErrorSummary): string => row.api_key_name ?? row.client ?? '—'
const upstreamModelLabel = (row: UpstreamErrorSummary): string => row.upstream_model ?? '—'
const routeModelLabel = (row: UpstreamErrorSummary): string => row.model_display_name ?? row.model ?? '—'
const statusLabel = (row: UpstreamErrorSummary): string =>
  row.error.status_code == null ? '—' : String(row.error.status_code)
const errorLabel = (row: UpstreamErrorSummary): string => row.error.message ?? row.error.code ?? '—'
const durationLabel = (row: UpstreamErrorSummary): string =>
  row.duration_ms == null ? '—' : formatDuration(Number(row.duration_ms))

// 时间、供应商、状态与耗时保持紧凑；多出的宽度只留给错误列，两个模型列不抢占空间。
const columns = helper.columns([
  helper.accessor('started_at', {
    header: () => m.failed_request_time(),
    cell: ({ row }) => renderSnippet(requestTime, row.original),
    size: 149,
    enableSorting: false,
    meta: { width: 'compact', cellClass: 'font-technical text-xs tabular-nums' },
  }),
  helper.accessor('provider_name', {
    header: () => m.observation_upstream_provider(),
    size: 76,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, row.original.provider_name),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(upstreamModelLabel, {
    id: 'upstream-model',
    header: () => m.observation_upstream_model(),
    size: 96,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, upstreamModelLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(routeModelLabel, {
    id: 'route-model',
    header: () => m.observation_upstream_route_model(),
    size: 84,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, routeModelLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(clientLabel, {
    id: 'client',
    header: () => m.failed_request_client(),
    size: 84,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(clippedCell, clientLabel(row.original)),
    meta: { width: 'compact', cellClass: 'max-w-0' },
  }),
  helper.accessor(statusLabel, {
    id: 'status',
    header: () => m.observation_upstream_status(),
    size: 56,
    enableSorting: false,
    meta: { width: 'compact', align: 'end', cellClass: 'font-technical text-xs tabular-nums' },
  }),
  helper.accessor(errorLabel, {
    id: 'error',
    header: () => m.failed_request_error(),
    size: 140,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(errorCell, row.original),
    meta: { width: 'fill', cellClass: 'max-w-0' },
  }),
  helper.accessor('outcome', {
    header: () => m.observation_upstream_result(),
    cell: ({ row }) => renderSnippet(outcomeCell, row.original),
    size: 140,
    enableSorting: false,
    meta: { width: 'compact' },
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

{#snippet requestTime(error: UpstreamErrorSummary)}
  <button
    class="text-start underline-offset-4 hover:underline"
    onclick={(event) => {
      event.stopPropagation()
      onselect(error)
    }}>
    {formatLogTime(error.started_at)}
  </button>
{/snippet}

{#snippet errorCell(error: UpstreamErrorSummary)}
  <div class="min-w-0">
    <span class="block whitespace-normal break-words" title={error.error.message ?? error.error.code ?? undefined}>
      {errorLabel(error)}
    </span>
    <span class="block font-technical text-xs whitespace-normal break-words text-muted-foreground">
      {[error.error.code, failureOriginLabel(error.error.source)].filter(Boolean).join(' · ')}
    </span>
  </div>
{/snippet}

{#snippet outcomeCell(error: UpstreamErrorSummary)}
  <!-- 结果标签较长时在窄列内换行，避免挤占错误列导致技术错误码从中间断开。 -->
  <Badge
    variant={upstreamErrorOutcomeTone(error.outcome)}
    class="h-auto max-w-full whitespace-normal text-center leading-tight"
    >{upstreamErrorOutcomeLabel(error.outcome)}</Badge>
{/snippet}

<div class="upstream-error-table">
  <div class="desktop-table">
    <DataTable
      data={items}
      {columns}
      {labels}
      {loading}
      ariaLabel={m.observation_upstream_errors()}
      getRowId={(row: UpstreamErrorSummary) => row.id}
      class="isolate flex-1"
      stickyHeader
      selectionMode="single"
      selectOnRowClick={false}
      onRowSelect={(row: DataTableRow<UpstreamErrorSummary>) => onselect(row.original)}
      onRowClick={({ row }: DataTableRowPointerEvent<UpstreamErrorSummary>) => onselect(row.original)} />
  </div>
  <!-- 窄工作区：先上游模型、结果、实际错误与时间，再列出供应商、路由模型、客户端、状态和耗时。 -->
  <ul class="mobile-list" aria-label={m.observation_upstream_errors()} aria-busy={loading || undefined}>
    {#each items as error (error.id)}
      <!-- 行本身只响应指针；键盘用户通过行内时间按钮打开详情，避免在按钮内嵌套按钮。 -->
      <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
      <li class="failure-row" onclick={() => onselect(error)}>
        <div class="failure-head">
          <p class="failure-model">{upstreamModelLabel(error)}</p>
          <div class="failure-head-meta">
            {@render outcomeCell(error)}
            {@render requestTime(error)}
          </div>
        </div>
        <p class="failure-error">{errorLabel(error)}</p>
        <p class="font-technical text-xs text-muted-foreground">
          {[error.error.code, error.error.status_code == null ? null : `HTTP ${error.error.status_code}`]
            .filter(Boolean)
            .join(' · ')}
        </p>
        <dl class="failure-facts">
          <dt>{m.observation_upstream_provider()}</dt>
          <dd>{error.provider_name}</dd>
          <dt>{m.observation_upstream_route_model()}</dt>
          <dd>{routeModelLabel(error)}</dd>
          <dt>{m.failed_request_client()}</dt>
          <dd>{clientLabel(error)}</dd>
          <dt>{m.failed_request_duration()}</dt>
          <dd class="font-technical tabular-nums">{durationLabel(error)}</dd>
        </dl>
      </li>
    {/each}
  </ul>
</div>

<style>
.upstream-error-table {
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
@container route-page (max-width: 52rem) {
  .desktop-table {
    display: none;
  }
  .mobile-list {
    display: block;
  }
}
</style>
