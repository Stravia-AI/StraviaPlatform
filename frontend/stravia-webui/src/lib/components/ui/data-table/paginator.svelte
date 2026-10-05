<script lang="ts" generics="TData extends RowData">
import type { PaginationState, RowData } from '@tanstack/svelte-table'
import ArrowLeftToLineIcon from '@lucide/svelte/icons/arrow-left-to-line'
import ArrowRightToLineIcon from '@lucide/svelte/icons/arrow-right-to-line'
import ChevronLeftIcon from '@lucide/svelte/icons/chevron-left'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'

import { Button } from '$lib/components/ui/button'
import * as Pagination from '$lib/components/ui/pagination'
import * as Select from '$lib/components/ui/select'
import type { DataTable, DataTableLabels } from './data-table.js'

type PaginationPageItem = { key: string } & ({ type: 'page'; value: number } | { type: 'ellipsis' })

interface Props {
  table: DataTable<TData>
  pagination: PaginationState
  resolvedLabels: DataTableLabels
  pageSizeOptions: readonly number[]
  pageCount: number
}

let { table, pagination, resolvedLabels, pageSizeOptions, pageCount }: Props = $props()
</script>

<div class="flex flex-wrap items-center justify-end gap-3" data-slot="data-table-paginator">
  <div class="flex items-center gap-2">
    <span class="text-sm text-muted-foreground">{resolvedLabels.rowsPerPage}</span>
    <Select.Root
      type="single"
      bind:value={() => String(pagination.pageSize), (value) => table.setPageSize(Number(value))}>
      <Select.Trigger class="h-10 w-20" aria-label={resolvedLabels.rowsPerPage}>{pagination.pageSize}</Select.Trigger>
      <Select.Content>
        <Select.Group>
          {#each pageSizeOptions as option (option)}
            <Select.Item value={String(option)} label={String(option)}>{option}</Select.Item>
          {/each}
        </Select.Group>
      </Select.Content>
    </Select.Root>
  </div>
  <span class="min-w-24 text-center text-sm text-muted-foreground tabular-nums">
    {resolvedLabels.pageStatus(pagination.pageIndex + 1, pageCount)}
  </span>
  <Pagination.Root
    class="mx-0 w-auto"
    count={table.getRowCount()}
    perPage={pagination.pageSize}
    bind:page={() => pagination.pageIndex + 1, (page) => table.setPageIndex(page - 1)}
    aria-label={resolvedLabels.pageStatus(pagination.pageIndex + 1, pageCount)}>
    {#snippet children({ pages, currentPage }: { pages: PaginationPageItem[]; currentPage: number })}
      <Pagination.Content class="flex-wrap">
        <Pagination.Item>
          <Button
            type="button"
            variant="outline"
            size="icon"
            class="size-10"
            aria-label={resolvedLabels.firstPage}
            disabled={!table.getCanPreviousPage()}
            onclick={() => table.firstPage()}>
            <ArrowLeftToLineIcon />
          </Button>
        </Pagination.Item>
        <Pagination.Item>
          <Pagination.Previous aria-label={resolvedLabels.previousPage} disabled={!table.getCanPreviousPage()}>
            <ChevronLeftIcon />
          </Pagination.Previous>
        </Pagination.Item>
        {#each pages as page (page.key)}
          <Pagination.Item>
            {#if page.type === 'ellipsis'}
              <Pagination.Ellipsis />
            {:else}
              <Pagination.Link
                {page}
                isActive={currentPage === page.value}
                aria-label={resolvedLabels.pageStatus((page as { value: number }).value, pageCount)}>
                {page.value}
              </Pagination.Link>
            {/if}
          </Pagination.Item>
        {/each}
        <Pagination.Item>
          <Pagination.Next aria-label={resolvedLabels.nextPage} disabled={!table.getCanNextPage()}>
            <ChevronRightIcon />
          </Pagination.Next>
        </Pagination.Item>
        <Pagination.Item>
          <Button
            type="button"
            variant="outline"
            size="icon"
            class="size-10"
            aria-label={resolvedLabels.lastPage}
            disabled={!table.getCanNextPage()}
            onclick={() => table.lastPage()}>
            <ArrowRightToLineIcon />
          </Button>
        </Pagination.Item>
      </Pagination.Content>
    {/snippet}
  </Pagination.Root>
</div>
