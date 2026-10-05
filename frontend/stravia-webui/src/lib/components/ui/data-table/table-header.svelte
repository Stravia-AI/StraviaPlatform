<script lang="ts" generics="TData extends RowData">
import { FlexRender, type RowData } from '@tanstack/svelte-table'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowUpDownIcon from '@lucide/svelte/icons/arrow-up-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'

import { cn } from '$lib/utils.js'
import { Button } from '$lib/components/ui/button'
import { Checkbox } from '$lib/components/ui/checkbox'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import * as Table from '$lib/components/ui/table'
import FilterMenu from './filter-menu.svelte'
import type {
  DataTable,
  DataTableLabels,
  DataTableSortMode,
  DataTableFilterDisplay,
  DataTableFilterGroup,
  DataTableColumnFilter,
  DataTableFilterOption,
  DataTableFilterMatchMode,
  DataTableFilterOperator,
} from './data-table.js'
import type { DataTableColumnHandle, DataTableRenderStyles } from './render-types.js'

interface Props {
  ref: HTMLTableSectionElement
  table: DataTable<TData>
  headerGroups: ReturnType<DataTable<TData>['getHeaderGroups']>
  visibleLeafColumns: ReturnType<DataTable<TData>['getVisibleLeafColumns']>
  styles: DataTableRenderStyles<TData>
  resolvedLabels: DataTableLabels
  showGridlines: boolean
  stickyHeader: boolean
  reorderableRows: boolean
  hasSelectionControl: boolean
  hasExpansionControl: boolean
  hasEditControl: boolean
  controlRowSpan: number
  reorderableColumns: boolean
  sortMode: DataTableSortMode
  filterDisplay: DataTableFilterDisplay
  filterDraft: DataTableFilterGroup | undefined
  openFilterColumnId: string | undefined
  allFilterValue: string
  resizableColumns: boolean
  startColumnDrag: (columnId: string) => void
  sortAriaLabel: (column: DataTableColumnHandle<TData>) => string
  handleColumnDrop: (targetId: string) => void
  resizeColumnByKeyboard: (event: KeyboardEvent, column: DataTableColumnHandle<TData>) => void
  selectFilterOptions: (column: DataTableColumnHandle<TData>) => readonly DataTableFilterOption[]
  updateNumberFilter: (column: DataTableColumnHandle<TData>, edge: 0 | 1, raw: string) => void
  clearColumnFilter: (column: DataTableColumnHandle<TData>) => void
  applyColumnFilter: (column: DataTableColumnHandle<TData>) => void
  setFilterMenuOpen: (column: DataTableColumnHandle<TData>, filter: DataTableColumnFilter, open: boolean) => void
  textFilterMatchModes: (filter: DataTableColumnFilter) => readonly DataTableFilterMatchMode[]
  updateFilterOperator: (operator: DataTableFilterOperator) => void
  updateFilterConstraint: (
    index: number,
    update: Partial<{ value: unknown; matchMode: DataTableFilterMatchMode }>,
  ) => void
  addFilterConstraint: (filter: DataTableColumnFilter) => void
  removeFilterConstraint: (index: number) => void
  updateDraftNumberFilter: (index: number, edge: 0 | 1, raw: string) => void
}

let {
  ref = $bindable(),
  table,
  headerGroups,
  visibleLeafColumns,
  styles,
  resolvedLabels,
  showGridlines,
  stickyHeader,
  reorderableRows,
  hasSelectionControl,
  hasExpansionControl,
  hasEditControl,
  controlRowSpan,
  reorderableColumns,
  sortMode,
  filterDisplay,
  filterDraft,
  openFilterColumnId,
  allFilterValue,
  resizableColumns,
  startColumnDrag,
  sortAriaLabel,
  handleColumnDrop,
  resizeColumnByKeyboard,
  selectFilterOptions,
  updateNumberFilter,
  clearColumnFilter,
  applyColumnFilter,
  setFilterMenuOpen,
  textFilterMatchModes,
  updateFilterOperator,
  updateFilterConstraint,
  addFilterConstraint,
  removeFilterConstraint,
  updateDraftNumberFilter,
}: Props = $props()
</script>

<Table.Header
  bind:ref
  class={cn(
    'bg-[var(--data-table-header-background)] shadow-[0_1px_0_var(--border)] [&_[data-slot=table-head]]:bg-[var(--data-table-header-background)] [&_[data-slot=table-head]]:text-[0.8rem] [&_[data-slot=table-head]]:text-muted-foreground',
    stickyHeader && 'sticky top-0 z-20',
  )}>
  {#each headerGroups as headerGroup, headerRowIndex (headerGroup.id)}
    <Table.Row class="border-border/50 hover:bg-transparent">
      {#if headerRowIndex === 0}
        {#if reorderableRows}
          <Table.Head
            rowspan={controlRowSpan}
            class={cn('w-10', styles.sizeClass('head'), showGridlines && 'border-e')}>
            <span class="sr-only">{resolvedLabels.reorderRow(0)}</span>
          </Table.Head>
        {/if}
        {#if hasSelectionControl}
          <Table.Head
            rowspan={controlRowSpan}
            class={cn('w-10', styles.sizeClass('head'), showGridlines && 'border-e')}>
            <Checkbox
              aria-label={resolvedLabels.selectAllRows}
              indeterminate={table.getIsSomePageRowsSelected() && !table.getIsAllPageRowsSelected()}
              bind:checked={
                () => table.getIsAllPageRowsSelected(), (value) => table.toggleAllPageRowsSelected(Boolean(value))
              } />
          </Table.Head>
        {/if}
        {#if hasExpansionControl}
          <Table.Head
            rowspan={controlRowSpan}
            class={cn('w-10', styles.sizeClass('head'), showGridlines && 'border-e')} />
        {/if}
        {#if hasEditControl}
          <Table.Head
            rowspan={controlRowSpan}
            class={cn('w-20', styles.sizeClass('head'), showGridlines && 'border-e')}>
            <span class="sr-only">{resolvedLabels.editRow(0)}</span>
          </Table.Head>
        {/if}
      {/if}
      {#each headerGroup.headers as header (header.id)}
        <Table.Head
          colspan={header.colSpan}
          rowspan={header.rowSpan}
          draggable={reorderableColumns && header.column.columns.length === 0}
          aria-sort={header.column.getIsSorted() === 'asc'
            ? 'ascending'
            : header.column.getIsSorted() === 'desc'
              ? 'descending'
              : header.column.getCanSort()
                ? 'none'
                : undefined}
          aria-label={reorderableColumns ? resolvedLabels.reorderColumn(styles.columnLabel(header.column)) : undefined}
          style={styles.columnInlineStyle(header.column, true)}
          class={cn(
            'relative',
            styles.sizeClass('head'),
            showGridlines && 'border-e last:border-e-0',
            styles.alignClass(header.column),
            header.column.columnDef.meta?.headerClass,
            reorderableColumns && header.column.columns.length === 0 && 'cursor-grab active:cursor-grabbing',
          )}
          ondragstart={reorderableColumns ? () => startColumnDrag(header.column.id) : undefined}
          ondragover={reorderableColumns ? (event: DragEvent) => event.preventDefault() : undefined}
          ondrop={reorderableColumns ? () => handleColumnDrop(header.column.id) : undefined}>
          {#if !header.isPlaceholder}
            {@const filter = header.column.columnDef.meta?.filter}
            <div
              class={cn(
                'flex min-w-0 items-center gap-1',
                header.column.columnDef.meta?.align === 'end' ? 'justify-end' : 'justify-between',
              )}>
              {#if header.column.getCanSort()}
                <Button
                  variant="ghost"
                  size="sm"
                  class={cn(
                    'group/sort -mx-2 min-w-0 gap-1.5 px-2 text-inherit',
                    header.column.getIsSorted() && 'text-foreground',
                    header.column.columnDef.meta?.align === 'end' && 'ms-auto',
                  )}
                  aria-label={sortAriaLabel(header.column)}
                  onclick={header.column.getToggleSortingHandler()}>
                  <FlexRender {header} />
                  {#if header.column.getIsSorted() === 'asc'}
                    <ArrowUpIcon data-icon="inline-end" />
                  {:else if header.column.getIsSorted() === 'desc'}
                    <ArrowDownIcon data-icon="inline-end" />
                  {:else}
                    <ArrowUpDownIcon
                      data-icon="inline-end"
                      class="opacity-40 transition-opacity group-hover/sort:opacity-100 group-focus-visible/sort:opacity-100" />
                  {/if}
                  {#if sortMode === 'multiple' && header.column.getSortIndex() >= 0}
                    <span class="font-technical text-[0.65rem] text-muted-foreground"
                      >{header.column.getSortIndex() + 1}</span>
                  {/if}
                </Button>
              {:else}
                <FlexRender {header} />
              {/if}
              {#if filterDisplay === 'menu' && filter && header.column.columns.length === 0}
                <FilterMenu
                  column={header.column}
                  {filter}
                  draft={filterDraft}
                  labels={resolvedLabels}
                  columnName={styles.columnLabel(header.column)}
                  open={openFilterColumnId === header.column.id}
                  {allFilterValue}
                  selectOptions={selectFilterOptions(header.column)}
                  textMatchModes={textFilterMatchModes(filter)}
                  onOpenChange={(open: boolean) => setFilterMenuOpen(header.column, filter, open)}
                  onUpdateOperator={updateFilterOperator}
                  onUpdateConstraint={updateFilterConstraint}
                  onAddConstraint={() => addFilterConstraint(filter)}
                  onRemoveConstraint={removeFilterConstraint}
                  onUpdateNumber={updateDraftNumberFilter}
                  onClear={() => clearColumnFilter(header.column)}
                  onApply={() => applyColumnFilter(header.column)} />
              {/if}
            </div>
            {#if resizableColumns && header.column.getCanResize()}
              <button
                type="button"
                aria-label={resolvedLabels.resizeColumn(styles.columnLabel(header.column))}
                class={cn(
                  'absolute inset-y-0 w-2 cursor-col-resize touch-none select-none outline-none after:absolute after:inset-y-1 after:start-1/2 after:w-px after:bg-transparent hover:after:bg-border/80 focus-visible:after:w-0.5 focus-visible:after:bg-ring',
                  header.column.id === visibleLeafColumns[visibleLeafColumns.length - 1]?.id
                    ? 'end-0 after:hidden'
                    : '-end-1',
                  header.column.getIsResizing() && 'after:w-0.5 after:bg-ring',
                )}
                onmousedown={header.getResizeHandler()}
                ontouchstart={header.getResizeHandler()}
                onkeydown={(event) => resizeColumnByKeyboard(event, header.column)}
                ondblclick={() => header.column.resetSize()}></button>
            {/if}
          {/if}
        </Table.Head>
      {/each}
    </Table.Row>
  {/each}
  {#if filterDisplay === 'row'}
    <Table.Row class="border-border/50 hover:bg-transparent">
      {#each visibleLeafColumns as column (column.id)}
        {@const filter = column.columnDef.meta?.filter}
        <Table.Head
          style={styles.columnInlineStyle(column, true)}
          class={cn(styles.sizeClass('head'), showGridlines && 'border-e last:border-e-0')}>
          {#if filter?.variant === 'text'}
            <Input
              class="h-8 min-w-28"
              value={(column.getFilterValue() as string | undefined) ?? ''}
              placeholder={filter.placeholder ?? styles.columnLabel(column)}
              aria-label={filter.placeholder ?? styles.columnLabel(column)}
              oninput={(event: Event) =>
                column.setFilterValue((event.currentTarget as HTMLInputElement).value || undefined)} />
          {:else if filter?.variant === 'select'}
            <Select.Root
              type="single"
              bind:value={
                () => (column.getFilterValue() as string | undefined) ?? allFilterValue,
                (value) => column.setFilterValue(value === allFilterValue ? undefined : value)
              }>
              <Select.Trigger class="h-8 min-w-28">
                {filter.options?.find((option) => option.value === column.getFilterValue())?.label ??
                  (column.getFilterValue() == null
                    ? (filter.allLabel ?? resolvedLabels.allValues)
                    : (column.getFilterValue() as string))}
              </Select.Trigger>
              <Select.Content>
                <Select.Group>
                  <Select.Item value={allFilterValue} label={filter.allLabel ?? resolvedLabels.allValues}>
                    {filter.allLabel ?? resolvedLabels.allValues}
                  </Select.Item>
                  {#each selectFilterOptions(column) as option (option.value)}
                    <Select.Item value={option.value} label={option.label}>{option.label}</Select.Item>
                  {/each}
                </Select.Group>
              </Select.Content>
            </Select.Root>
          {:else if filter?.variant === 'number-range'}
            {@const range = (column.getFilterValue() as [number | undefined, number | undefined] | undefined) ?? []}
            <div class="flex min-w-48 gap-1">
              <Input
                class="h-8 min-w-20"
                type="number"
                value={range[0] ?? ''}
                placeholder={filter.minPlaceholder ?? resolvedLabels.minimum}
                aria-label={filter.minPlaceholder ?? resolvedLabels.minimum}
                oninput={(event: Event) =>
                  updateNumberFilter(column, 0, (event.currentTarget as HTMLInputElement).value)} />
              <Input
                class="h-8 min-w-20"
                type="number"
                value={range[1] ?? ''}
                placeholder={filter.maxPlaceholder ?? resolvedLabels.maximum}
                aria-label={filter.maxPlaceholder ?? resolvedLabels.maximum}
                oninput={(event: Event) =>
                  updateNumberFilter(column, 1, (event.currentTarget as HTMLInputElement).value)} />
            </div>
          {:else if filter?.variant === 'custom'}
            {@render filter.content(column.getFilterValue(), (value) => column.setFilterValue(value))}
          {/if}
        </Table.Head>
      {/each}
    </Table.Row>
  {/if}
</Table.Header>
