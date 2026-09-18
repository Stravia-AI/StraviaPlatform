<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { renderSnippet } from '@tanstack/svelte-table'
import { formatDuration, formatLogTime } from '$lib/format'
import { getDataTableLabels } from '$lib/data-table-labels'
import { failureOriginLabel } from '$lib/observation-labels'
import type { FailedRequestSummary } from '$lib/types'
import { DataTable, createDataTableColumnHelper, type DataTableRowPointerEvent } from '$lib/components/ui/data-table'

let {
  items,
  loading,
  onselect,
}: { items: FailedRequestSummary[]; loading: boolean; onselect: (request: FailedRequestSummary) => void } = $props()
const labels = $derived(getDataTableLabels())
const helper = createDataTableColumnHelper<FailedRequestSummary>()
const columns = helper.columns([
  helper.accessor('started_at', {
    header: () => m.failed_request_time(),
    cell: ({ row }) => renderSnippet(requestTime, row.original),
    size: 190,
    enableSorting: false,
    meta: { cellClass: 'font-technical text-xs tabular-nums' },
  }),
  helper.accessor(
    (row) =>
      row.api_key_name ??
      row.client ??
      (row.kind === 'rejection' && !row.observation_gap ? m.failed_request_unauthenticated() : '—'),
    { id: 'source', header: () => m.failed_request_client(), size: 170, enableSorting: false },
  ),
  helper.accessor((row) => row.model_display_name ?? row.model ?? '—', {
    id: 'model',
    header: () => m.common_model(),
    size: 160,
    enableSorting: false,
  }),
  helper.accessor((row) => row.services.map((service) => service.name).join(', ') || '—', {
    id: 'services',
    header: () => m.common_model_service(),
    size: 180,
    enableSorting: false,
  }),
  helper.accessor((row) => failureOriginLabel(row.error.source) ?? '—', {
    id: 'origin',
    header: () => m.failed_request_origin(),
    size: 120,
    enableSorting: false,
  }),
  helper.accessor((row) => row.error.message ?? row.error.code ?? '—', {
    id: 'error',
    header: () => m.failed_request_error(),
    size: 280,
    enableSorting: false,
    cell: ({ row }) => renderSnippet(errorCell, row.original),
  }),
  helper.accessor('duration_ms', {
    header: () => m.failed_request_duration(),
    size: 100,
    enableSorting: false,
    cell: (context) => (context.getValue() == null ? '—' : formatDuration(Number(context.getValue()))),
    meta: { align: 'end', cellClass: 'font-technical text-xs tabular-nums' },
  }),
])
</script>

{#snippet requestTime(request: FailedRequestSummary)}
  <button
    class="min-h-10 text-start underline-offset-4 hover:underline"
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
      {request.error.message ?? request.error.code ?? '—'}
    </span>
    {#if request.error.status_code !== null}
      <span class="font-technical text-xs text-muted-foreground">HTTP {request.error.status_code}</span>
    {/if}
  </div>
{/snippet}

<DataTable
  data={items}
  {columns}
  {labels}
  {loading}
  ariaLabel={m.observation_failed_requests()}
  getRowId={(row: FailedRequestSummary) => `${row.kind}:${row.id}`}
  class="isolate min-h-0 flex-1"
  scrollHeight="100%"
  stickyHeader
  onRowClick={({ row }: DataTableRowPointerEvent<FailedRequestSummary>) => onselect(row.original)} />
