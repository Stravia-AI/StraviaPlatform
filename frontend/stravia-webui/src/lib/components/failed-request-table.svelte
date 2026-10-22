<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { renderSnippet } from '@tanstack/svelte-table'
import { formatDuration, formatLogTime } from '#lib/format.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import { failureOriginLabel } from '#lib/observation-labels.js'
import type { FailedRequestSummary } from '#lib/types.js'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableRowPointerEvent,
} from '#lib/components/ui/data-table/index.js'

let {
  items,
  loading,
  onselect,
}: { items: FailedRequestSummary[]; loading: boolean; onselect: (request: FailedRequestSummary) => void } = $props()
const labels = $derived(getDataTableLabels())
const helper = createDataTableColumnHelper<FailedRequestSummary>()

function sourceLabel(row: FailedRequestSummary): string {
  return (
    row.api_key_name ??
    row.client ??
    (row.kind === 'rejection' && !row.observation_gap ? m.failed_request_unauthenticated() : '—')
  )
}
const modelLabel = (row: FailedRequestSummary): string => row.model_display_name ?? row.model ?? '—'
const servicesLabel = (row: FailedRequestSummary): string =>
  row.services.map((service) => service.name).join(', ') || '—'
const originLabel = (row: FailedRequestSummary): string => failureOriginLabel(row.error.source) ?? '—'
const errorLabel = (row: FailedRequestSummary): string => row.error.message ?? row.error.code ?? '—'
const durationLabel = (row: FailedRequestSummary): string =>
  row.duration_ms == null ? '—' : formatDuration(Number(row.duration_ms))

// 时间、来源、耗时保持紧凑；多出的宽度只交给模型、供应商名称与错误三列。
const columns = helper.columns([
  helper.accessor('started_at', {
    header: () => m.failed_request_time(),
    cell: ({ row }) => renderSnippet(requestTime, row.original),
    size: 170,
    enableSorting: false,
    meta: { width: 'compact', cellClass: 'font-technical text-xs tabular-nums' },
  }),
  helper.accessor(sourceLabel, {
    id: 'source',
    header: () => m.failed_request_client(),
    size: 150,
    enableSorting: false,
    meta: { width: 'compact' },
  }),
  helper.accessor(modelLabel, {
    id: 'model',
    header: () => m.common_model(),
    size: 160,
    enableSorting: false,
    meta: { width: 'fill' },
  }),
  helper.accessor(servicesLabel, {
    id: 'services',
    header: () => m.common_model_service(),
    size: 180,
    enableSorting: false,
    meta: { width: 'fill' },
  }),
  helper.accessor(originLabel, {
    id: 'origin',
    header: () => m.failed_request_origin(),
    size: 110,
    enableSorting: false,
    meta: { width: 'compact' },
  }),
  helper.accessor(errorLabel, {
    id: 'error',
    header: () => m.failed_request_error(),
    size: 320,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(errorCell, row.original),
    meta: { width: 'fill' },
  }),
  helper.accessor('duration_ms', {
    header: () => m.failed_request_duration(),
    size: 90,
    enableSorting: false,
    cell: ({ row }) => durationLabel(row.original),
    meta: { width: 'compact', align: 'end', cellClass: 'font-technical text-xs tabular-nums' },
  }),
])
</script>

{#snippet requestTime(request: FailedRequestSummary)}
  <button
    class="text-start underline-offset-4 hover:underline"
    onclick={(event) => {
      event.stopPropagation()
      onselect(request)
    }}>
    {formatLogTime(request.started_at)}
  </button>
{/snippet}

{#snippet errorCell(request: FailedRequestSummary)}
  <div class="min-w-0">
    <span class="block truncate" title={request.error.message ?? request.error.code ?? undefined}>
      {errorLabel(request)}
    </span>
    {#if request.error.status_code !== null}
      <span class="font-technical text-xs text-muted-foreground">HTTP {request.error.status_code}</span>
    {/if}
  </div>
{/snippet}

<div class="failed-request-table">
  <div class="desktop-table">
    <DataTable
      data={items}
      {columns}
      {labels}
      {loading}
      ariaLabel={m.observation_failed_requests()}
      getRowId={(row: FailedRequestSummary) => `${row.kind}:${row.id}`}
      class="isolate flex-1"
      workbench
      stickyHeader
      onRowClick={({ row }: DataTableRowPointerEvent<FailedRequestSummary>) => onselect(row.original)} />
  </div>
  <!-- 窄工作区：先模型、实际错误与时间，再以 dl 列出客户端／密钥、供应商、来源与耗时。 -->
  <ul class="mobile-list" aria-label={m.observation_failed_requests()} aria-busy={loading || undefined}>
    {#each items as request (`${request.kind}:${request.id}`)}
      <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
      <li class="failure-row" onclick={() => onselect(request)}>
        <div class="failure-head">
          <p class="failure-model">{modelLabel(request)}</p>
          {@render requestTime(request)}
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
.failed-request-table {
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
.failure-row:hover {
  background: color-mix(in oklab, var(--muted) 45%, var(--background));
}
.failure-head {
  display: flex;
  min-width: 0;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.75rem;
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
  display: -webkit-box;
  overflow: hidden;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 3;
  line-clamp: 3;
  font-size: 0.875rem;
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
