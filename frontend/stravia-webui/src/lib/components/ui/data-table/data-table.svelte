<script lang="ts" generics="TData extends RowData">
import {
  FlexRender,
  createTable,
  functionalUpdate,
  type Column,
  type ColumnFiltersState,
  type ColumnOrderState,
  type ColumnPinningState,
  type ColumnSizingState,
  type ColumnVisibilityState,
  type ExpandedState,
  type GroupingState,
  type PaginationState,
  type RowData,
  type RowPinningState,
  type RowSelectionState,
  type SortingState,
  type Updater,
} from '@tanstack/svelte-table'
import { onMount, type Snippet } from 'svelte'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowLeftToLineIcon from '@lucide/svelte/icons/arrow-left-to-line'
import ArrowRightToLineIcon from '@lucide/svelte/icons/arrow-right-to-line'
import ArrowUpDownIcon from '@lucide/svelte/icons/arrow-up-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import ChevronLeftIcon from '@lucide/svelte/icons/chevron-left'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import ChevronUpIcon from '@lucide/svelte/icons/chevron-up'
import CheckIcon from '@lucide/svelte/icons/check'
import DownloadIcon from '@lucide/svelte/icons/download'
import FunnelXIcon from '@lucide/svelte/icons/funnel-x'
import GripVerticalIcon from '@lucide/svelte/icons/grip-vertical'
import PencilIcon from '@lucide/svelte/icons/pencil'
import SearchIcon from '@lucide/svelte/icons/search'
import XIcon from '@lucide/svelte/icons/x'

import { cn } from '$lib/utils.js'
import { Button } from '$lib/components/ui/button'
import { Checkbox } from '$lib/components/ui/checkbox'
import * as Empty from '$lib/components/ui/empty'
import { Input } from '$lib/components/ui/input'
import * as InputGroup from '$lib/components/ui/input-group'
import * as Select from '$lib/components/ui/select'
import { Skeleton } from '$lib/components/ui/skeleton'
import { Spinner } from '$lib/components/ui/spinner'
import * as Table from '$lib/components/ui/table'
import ColumnMenu from './column-menu.svelte'
import FilterMenu from './filter-menu.svelte'
import { exportDataTableCsv } from './export.js'
import { parseDataTableState, serializeDataTableState } from './state-persistence.js'
import { dataTableVirtualRange } from './virtual-rows.js'
import {
  dataTableFeatures,
  defaultDataTableLabels,
  type DataTable,
  type DataTableCell,
  type DataTableCellEditEvent,
  type DataTableColumn,
  type DataTableColumnFilter,
  type DataTableFilterDisplay,
  type DataTableFilterGroup,
  type DataTableFilterMatchMode,
  type DataTableFilterOperator,
  type DataTableEditMode,
  type DataTableEditingCell,
  type DataTableLabels,
  type DataTablePaginatorPosition,
  type DataTableRow,
  type DataTableRowPointerEvent,
  type DataTableRowReorderEvent,
  type DataTableSelectionMode,
  type DataTableSize,
  type DataTableSortMode,
  type DataTableState,
  type DataTableStateStorage,
  type DataTableExportOptions,
  type DataTableVirtualScrollOptions,
} from './data-table.js'

interface Props {
  data: TData[]
  columns: DataTableColumn<TData>[]
  getRowId?: (row: TData, index: number, parent?: DataTableRow<TData>) => string
  getSubRows?: (row: TData, index: number) => TData[] | undefined
  class?: string
  tableClass?: string
  ariaLabel?: string
  caption?: string
  size?: DataTableSize
  showGridlines?: boolean
  stripedRows?: boolean
  scrollHeight?: string
  virtualScrollOptions?: DataTableVirtualScrollOptions
  stickyHeader?: boolean
  sortMode?: DataTableSortMode
  removableSort?: boolean
  filterDisplay?: DataTableFilterDisplay
  globalFilterEnabled?: boolean
  globalFilterId?: string
  globalFilterPlaceholder?: string
  columnToggle?: boolean
  exportable?: boolean
  exportFilename?: string
  paginator?: boolean
  paginatorPosition?: DataTablePaginatorPosition
  pageSizeOptions?: readonly number[]
  selectionMode?: DataTableSelectionMode
  metaKeySelection?: boolean
  selectOnRowClick?: boolean
  contextMenu?: boolean
  expandedContent?: Snippet<[DataTableRow<TData>]>
  editMode?: DataTableEditMode
  cellEditor?: Snippet<[DataTableCell<TData>, () => void, () => void]>
  toolbar?: Snippet<[DataTable<TData>]>
  toolbarEnd?: Snippet<[DataTable<TData>]>
  empty?: Snippet
  loadingContent?: Snippet
  footer?: Snippet<[DataTable<TData>]>
  showColumnFooters?: boolean
  loading?: boolean
  loadingRows?: number
  resizableColumns?: boolean
  columnResizeMode?: 'onChange' | 'onEnd'
  reorderableColumns?: boolean
  reorderableRows?: boolean
  manualFiltering?: boolean
  manualSorting?: boolean
  manualPagination?: boolean
  manualGrouping?: boolean
  manualExpanding?: boolean
  rowCount?: number
  keepPinnedRows?: boolean
  stateKey?: string
  stateStorage?: DataTableStateStorage
  labels?: Partial<DataTableLabels>
  getExportValue?: (row: TData, columnId: string, value: unknown) => unknown
  rowClass?: (row: DataTableRow<TData>) => string | undefined
  rowStyle?: (row: DataTableRow<TData>) => string | undefined
  cellClass?: (cell: DataTableCell<TData>) => string | undefined
  onRowClick?: (event: DataTableRowPointerEvent<TData>) => void
  onRowDoubleClick?: (event: DataTableRowPointerEvent<TData>) => void
  onRowContextMenu?: (event: DataTableRowPointerEvent<TData>) => void
  onContextMenuSelectionChange?: (row: TData) => void
  onRowReorder?: (event: DataTableRowReorderEvent<TData>) => void
  onRowSelect?: (row: DataTableRow<TData>) => void
  onRowUnselect?: (row: DataTableRow<TData>) => void
  onSelectionChange?: (rows: TData[]) => void
  onRowExpand?: (row: DataTableRow<TData>) => void
  onRowCollapse?: (row: DataTableRow<TData>) => void
  onRowGroupExpand?: (row: DataTableRow<TData>) => void
  onRowGroupCollapse?: (row: DataTableRow<TData>) => void
  onCellEditInit?: (event: DataTableCellEditEvent<TData>) => void
  onCellEditSave?: (event: DataTableCellEditEvent<TData>) => void
  onCellEditCancel?: (event: DataTableCellEditEvent<TData>) => void
  onRowEditInit?: (row: DataTableRow<TData>) => void
  onRowEditSave?: (row: DataTableRow<TData>) => void
  onRowEditCancel?: (row: DataTableRow<TData>) => void
  onPageChange?: (pagination: PaginationState) => void
  onSortChange?: (sorting: SortingState) => void
  onFilterChange?: (filters: { columnFilters: ColumnFiltersState; globalFilter: string }) => void
  onColumnResize?: (sizing: ColumnSizingState) => void
  onColumnReorder?: (order: ColumnOrderState) => void
  onStateSave?: (state: DataTableState) => void
  onStateRestore?: (state: DataTableState) => void
  onExport?: (event: { filename: string; rowCount: number }) => void
  onStateChange?: (state: DataTableState) => void
  onStateError?: (error: Error) => void
  sorting?: SortingState
  columnFilters?: ColumnFiltersState
  globalFilter?: string
  pagination?: PaginationState
  rowSelection?: RowSelectionState
  contextMenuSelection?: string
  expanded?: ExpandedState
  editingCell?: DataTableEditingCell
  editingRows?: RowSelectionState
  grouping?: GroupingState
  columnVisibility?: ColumnVisibilityState
  columnOrder?: ColumnOrderState
  columnPinning?: ColumnPinningState
  columnSizing?: ColumnSizingState
  rowPinning?: RowPinningState
}

interface RenderedRow {
  row: DataTableRow<TData>
  region: 'top' | 'center' | 'bottom'
  regionIndex: number
  regionCount: number
  rowIndex: number
}

const allFilterValue = '__data_table_all_values__'
let {
  data,
  columns,
  getRowId,
  getSubRows,
  class: className,
  tableClass,
  ariaLabel,
  caption,
  size = 'default',
  showGridlines = false,
  stripedRows = false,
  scrollHeight,
  virtualScrollOptions,
  stickyHeader = false,
  sortMode = 'single',
  removableSort = true,
  filterDisplay = 'none',
  globalFilterEnabled = false,
  globalFilterId,
  globalFilterPlaceholder,
  columnToggle = false,
  exportable = false,
  exportFilename = 'table.csv',
  paginator = false,
  paginatorPosition = 'bottom',
  pageSizeOptions = [10, 25, 50, 100],
  selectionMode = 'none',
  metaKeySelection = true,
  selectOnRowClick = true,
  contextMenu = false,
  expandedContent,
  editMode = 'none',
  cellEditor,
  toolbar,
  toolbarEnd,
  empty,
  loadingContent,
  footer,
  showColumnFooters = false,
  loading = false,
  loadingRows = 5,
  resizableColumns = false,
  columnResizeMode = 'onChange',
  reorderableColumns = false,
  reorderableRows = false,
  manualFiltering = false,
  manualSorting = false,
  manualPagination = false,
  manualGrouping = false,
  manualExpanding = false,
  rowCount,
  keepPinnedRows = true,
  stateKey,
  stateStorage = 'local',
  labels = {},
  getExportValue,
  rowClass,
  rowStyle,
  cellClass,
  onRowClick,
  onRowDoubleClick,
  onRowContextMenu,
  onContextMenuSelectionChange,
  onRowReorder,
  onRowSelect,
  onRowUnselect,
  onSelectionChange,
  onRowExpand,
  onRowCollapse,
  onRowGroupExpand,
  onRowGroupCollapse,
  onCellEditInit,
  onCellEditSave,
  onCellEditCancel,
  onRowEditInit,
  onRowEditSave,
  onRowEditCancel,
  onPageChange,
  onSortChange,
  onFilterChange,
  onColumnResize,
  onColumnReorder,
  onStateSave,
  onStateRestore,
  onExport,
  onStateChange,
  onStateError,
  sorting = $bindable<SortingState>([]),
  columnFilters = $bindable<ColumnFiltersState>([]),
  globalFilter = $bindable(''),
  pagination = $bindable<PaginationState>({ pageIndex: 0, pageSize: 10 }),
  rowSelection = $bindable<RowSelectionState>({}),
  contextMenuSelection = $bindable(''),
  expanded = $bindable<ExpandedState>({}),
  editingCell = $bindable<DataTableEditingCell | undefined>(undefined),
  editingRows = $bindable<RowSelectionState>({}),
  grouping = $bindable<GroupingState>([]),
  columnVisibility = $bindable<ColumnVisibilityState>({}),
  columnOrder = $bindable<ColumnOrderState>([]),
  columnPinning = $bindable<ColumnPinningState>({ start: [], end: [] }),
  columnSizing = $bindable<ColumnSizingState>({}),
  rowPinning = $bindable<RowPinningState>({ top: [], bottom: [] }),
}: Props = $props()

let rootElement: HTMLDivElement
let viewportElement: HTMLDivElement
let selectionAnchorId: string | undefined
let draggedColumnId: string | undefined
let draggedRowId: string | undefined
let persistenceReady = $state(false)
let virtualScrollTop = $state(0)
let virtualViewportHeight = $state(0)
let openFilterColumnId = $state<string>()
let filterDraft = $state<DataTableFilterGroup>()
const resolvedLabels = $derived({ ...defaultDataTableLabels, ...labels })

function currentState(): DataTableState {
  return {
    sorting,
    columnFilters,
    globalFilter,
    pagination,
    rowSelection,
    expanded,
    grouping,
    columnVisibility,
    columnOrder,
    columnPinning,
    columnSizing,
    rowPinning,
  }
}

function notifyStateChange(): void {
  onStateChange?.(currentState())
}

function resetPaginationForDataChange(): void {
  if (!paginator || pagination.pageIndex === 0) return
  pagination = { ...pagination, pageIndex: 0 }
  onPageChange?.(pagination)
}

function reportStateError(cause: unknown): void {
  const error = cause instanceof Error ? cause : new Error(String(cause))
  if (onStateError) onStateError(error)
  else console.error('DataTable state persistence failed.', error)
}

function storage(): Storage {
  return stateStorage === 'session' ? window.sessionStorage : window.localStorage
}

function commitRowSelection(next: RowSelectionState): void {
  const previous = rowSelection
  rowSelection = next
  for (const row of table.getPreFilteredRowModel().flatRows) {
    const wasSelected = Boolean(previous[row.id])
    const isSelected = Boolean(next[row.id])
    if (wasSelected === isSelected) continue
    if (isSelected) onRowSelect?.(row)
    else onRowUnselect?.(row)
  }
  onSelectionChange?.(
    table
      .getPreFilteredRowModel()
      .flatRows.filter((row) => Boolean(next[row.id]))
      .map((row) => row.original),
  )
  notifyStateChange()
}

function isExpandedInState(state: ExpandedState, rowId: string): boolean {
  return state === true || Boolean(state[rowId])
}

function commitExpanded(next: ExpandedState): void {
  const previous = expanded
  expanded = next
  for (const row of table.getPreExpandedRowModel().flatRows) {
    const wasExpanded = isExpandedInState(previous, row.id)
    const isExpanded = isExpandedInState(next, row.id)
    if (wasExpanded === isExpanded) continue
    if (row.getIsGrouped()) {
      if (isExpanded) onRowGroupExpand?.(row)
      else onRowGroupCollapse?.(row)
    } else if (isExpanded) onRowExpand?.(row)
    else onRowCollapse?.(row)
  }
  notifyStateChange()
}

const table = createTable({
  features: dataTableFeatures,
  defaultColumn: {
    filterFn: 'dataTable',
    minSize: 0,
  },
  get data() {
    return data
  },
  get columns() {
    return columns
  },
  get getRowId() {
    return getRowId
  },
  get getSubRows() {
    return getSubRows
  },
  state: {
    get sorting() {
      return sorting
    },
    get columnFilters() {
      return columnFilters
    },
    get globalFilter() {
      return globalFilter
    },
    get pagination() {
      return pagination
    },
    get rowSelection() {
      return rowSelection
    },
    get expanded() {
      return expanded
    },
    get grouping() {
      return grouping
    },
    get columnVisibility() {
      return columnVisibility
    },
    get columnOrder() {
      return columnOrder
    },
    get columnPinning() {
      return columnPinning
    },
    get columnSizing() {
      return columnSizing
    },
    get rowPinning() {
      return rowPinning
    },
  },
  onSortingChange: (updater: Updater<SortingState>) => {
    sorting = functionalUpdate(updater, sorting)
    resetPaginationForDataChange()
    onSortChange?.(sorting)
    notifyStateChange()
  },
  onColumnFiltersChange: (updater: Updater<ColumnFiltersState>) => {
    columnFilters = functionalUpdate(updater, columnFilters)
    resetPaginationForDataChange()
    onFilterChange?.({ columnFilters, globalFilter })
    notifyStateChange()
  },
  onGlobalFilterChange: (updater: Updater<unknown>) => {
    globalFilter = String(functionalUpdate(updater, globalFilter) ?? '')
    resetPaginationForDataChange()
    onFilterChange?.({ columnFilters, globalFilter })
    notifyStateChange()
  },
  onPaginationChange: (updater: Updater<PaginationState>) => {
    pagination = functionalUpdate(updater, pagination)
    onPageChange?.(pagination)
    notifyStateChange()
  },
  onRowSelectionChange: (updater: Updater<RowSelectionState>) => {
    commitRowSelection(functionalUpdate(updater, rowSelection))
  },
  onExpandedChange: (updater: Updater<ExpandedState>) => {
    commitExpanded(functionalUpdate(updater, expanded))
  },
  onGroupingChange: (updater: Updater<GroupingState>) => {
    grouping = functionalUpdate(updater, grouping)
    resetPaginationForDataChange()
    notifyStateChange()
  },
  onColumnVisibilityChange: (updater: Updater<ColumnVisibilityState>) => {
    columnVisibility = functionalUpdate(updater, columnVisibility)
    notifyStateChange()
  },
  onColumnOrderChange: (updater: Updater<ColumnOrderState>) => {
    columnOrder = functionalUpdate(updater, columnOrder)
    onColumnReorder?.(columnOrder)
    notifyStateChange()
  },
  onColumnPinningChange: (updater: Updater<ColumnPinningState>) => {
    columnPinning = functionalUpdate(updater, columnPinning)
    notifyStateChange()
  },
  onColumnSizingChange: (updater: Updater<ColumnSizingState>) => {
    columnSizing = functionalUpdate(updater, columnSizing)
    onColumnResize?.(columnSizing)
    notifyStateChange()
  },
  onRowPinningChange: (updater: Updater<RowPinningState>) => {
    rowPinning = functionalUpdate(updater, rowPinning)
    notifyStateChange()
  },
  globalFilterFn: 'includesString',
  get enableGlobalFilter() {
    return globalFilterEnabled
  },
  get enableMultiSort() {
    return sortMode === 'multiple'
  },
  get enableSortingRemoval() {
    return removableSort
  },
  get enableRowSelection() {
    return selectionMode !== 'none'
  },
  get enableMultiRowSelection() {
    return selectionMode === 'multiple'
  },
  get enableColumnResizing() {
    return resizableColumns
  },
  get columnResizeMode() {
    return columnResizeMode
  },
  get manualFiltering() {
    return manualFiltering
  },
  get manualSorting() {
    return manualSorting
  },
  get manualPagination() {
    return manualPagination || !paginator
  },
  autoResetPageIndex: false,
  get manualGrouping() {
    return manualGrouping
  },
  get manualExpanding() {
    return manualExpanding
  },
  get rowCount() {
    return rowCount
  },
  get keepPinnedRows() {
    return keepPinnedRows
  },
  getRowCanExpand: (row) => Boolean(expandedContent || row.subRows.length > 0),
})

const headerGroups = $derived(table.getHeaderGroups())
const visibleLeafColumns = $derived(table.getVisibleLeafColumns())
const hasSelectionControl = $derived(selectionMode === 'multiple')
const hasExpansionControl = $derived(Boolean(expandedContent))
const hasEditControl = $derived(editMode === 'row' && Boolean(cellEditor))
const controlColumnCount = $derived(
  Number(hasSelectionControl) + Number(hasExpansionControl) + Number(hasEditControl) + Number(reorderableRows),
)
const renderedColumnCount = $derived(visibleLeafColumns.length + controlColumnCount)
const controlRowSpan = $derived(headerGroups.length + (filterDisplay === 'row' ? 1 : 0))
const hasConfiguredColumnFilters = $derived(
  visibleLeafColumns.some((column) => Boolean(column.columnDef.meta?.filter)),
)
const activeFilterCount = $derived(
  columnFilters.filter((entry) => isFilterValueActive(entry.value)).length + Number(Boolean(globalFilter.trim())),
)
const pageCount = $derived(Math.max(1, table.getPageCount()))
const visiblePageIndexes = $derived.by(() => {
  const linkCount = Math.min(5, pageCount)
  const first = Math.min(Math.max(0, pagination.pageIndex - Math.floor(linkCount / 2)), pageCount - linkCount)
  return Array.from({ length: linkCount }, (_, index) => first + index)
})
const showToolbar = $derived(
  Boolean(
    toolbar ||
      toolbarEnd ||
      globalFilterEnabled ||
      columnToggle ||
      exportable ||
      (filterDisplay !== 'none' && hasConfiguredColumnFilters),
  ),
)
const headerHeight = $derived(size === 'small' ? 32 : size === 'large' ? 48 : 40)
const rowHeight = $derived(size === 'small' ? 32 : size === 'large' ? 48 : 40)
const skeletonKeys = $derived(Array.from({ length: loadingRows }, (_, index) => `skeleton-${index}`))
const rowRegions = $derived.by(() => {
  const top = table.getTopRows()
  const center = table.getCenterRows()
  const bottom = table.getBottomRows()
  return {
    top: top.map(
      (row, regionIndex): RenderedRow => ({
        row,
        region: 'top',
        regionIndex,
        regionCount: top.length,
        rowIndex: regionIndex,
      }),
    ),
    center: center.map(
      (row, regionIndex): RenderedRow => ({
        row,
        region: 'center',
        regionIndex,
        regionCount: center.length,
        rowIndex: top.length + regionIndex,
      }),
    ),
    bottom: bottom.map(
      (row, regionIndex): RenderedRow => ({
        row,
        region: 'bottom',
        regionIndex,
        regionCount: bottom.length,
        rowIndex: top.length + center.length + regionIndex,
      }),
    ),
  }
})
const renderedRows = $derived([...rowRegions.top, ...rowRegions.center, ...rowRegions.bottom])
const virtualScrollEnabled = $derived(Boolean(virtualScrollOptions && scrollHeight))
const virtualRange = $derived(
  dataTableVirtualRange({
    enabled: virtualScrollEnabled,
    rowCount: rowRegions.center.length,
    scrollTop: virtualScrollTop,
    viewportHeight: virtualViewportHeight,
    itemSize: virtualScrollOptions?.itemSize ?? rowHeight,
    overscan: virtualScrollOptions?.overscan ?? 5,
  }),
)
const visibleCenterRows = $derived(rowRegions.center.slice(virtualRange.startIndex, virtualRange.endIndex))
const virtualTopPadding = $derived(virtualRange.topPadding)
const virtualBottomPadding = $derived(virtualRange.bottomPadding)

function isInteractiveTarget(target: EventTarget | null): boolean {
  return (
    target instanceof Element &&
    Boolean(target.closest('a, button, input, select, textarea, [role="button"], [role="menuitem"]'))
  )
}

function columnLabel(column: Column<typeof dataTableFeatures, TData, unknown>): string {
  const label = column.columnDef.meta?.label
  if (typeof label === 'function') return label()
  if (label) return label
  return typeof column.columnDef.header === 'string' ? column.columnDef.header : column.id
}

function alignClass(column: Column<typeof dataTableFeatures, TData, unknown>): string | undefined {
  const align = column.columnDef.meta?.align
  if (align === 'center') return 'text-center'
  if (align === 'end') return 'text-end'
  return undefined
}

function sizeClass(section: 'head' | 'cell'): string {
  if (size === 'small') return section === 'head' ? 'h-8 px-2 py-1' : 'px-2 py-1'
  if (size === 'large') return section === 'head' ? 'h-12 px-3 py-3' : 'px-3 py-3'
  return section === 'head' ? 'h-10 px-2' : 'p-2'
}

function columnInlineStyle(
  column: Column<typeof dataTableFeatures, TData, unknown>,
  header = false,
): string | undefined {
  const declarations: string[] = []
  if (resizableColumns || column.getIsPinned()) declarations.push(`width:${column.getSize()}px`)
  const pinned = column.getIsPinned()
  if (pinned) {
    declarations.push('position:sticky', 'background:var(--background)', `z-index:${header ? 30 : 10}`)
    if (pinned === 'start') {
      declarations.push(`inset-inline-start:${column.getStart('start')}px`)
      if (column.getIsLastColumn('start')) declarations.push('box-shadow:1px 0 0 var(--border)')
    } else {
      declarations.push(`inset-inline-end:${column.getAfter('end')}px`)
      if (column.getIsFirstColumn('end')) declarations.push('box-shadow:-1px 0 0 var(--border)')
    }
  }
  return declarations.length > 0 ? declarations.join(';') : undefined
}

function headerInlineStyle(
  column: Column<typeof dataTableFeatures, TData, unknown>,
  headerRowIndex: number,
): string | undefined {
  const declarations = columnInlineStyle(column, true)?.split(';') ?? []
  if (stickyHeader)
    declarations.push(
      'position:sticky',
      `top:${headerRowIndex * headerHeight}px`,
      'z-index:20',
      'background:var(--background)',
    )
  return declarations.length > 0 ? declarations.join(';') : undefined
}

function pinnedRowStyle(item: RenderedRow): string | undefined {
  if (item.region === 'center') return undefined
  const headerOffset = headerGroups.length * headerHeight + (filterDisplay === 'row' ? headerHeight : 0)
  if (item.region === 'top') {
    return `position:sticky;top:${headerOffset + item.regionIndex * rowHeight}px;z-index:5;background:var(--background)`
  }
  const reverseIndex = item.regionCount - item.regionIndex - 1
  return `position:sticky;bottom:${reverseIndex * rowHeight}px;z-index:5;background:var(--background)`
}

function renderedRowStyle(item: RenderedRow): string | undefined {
  const declarations = pinnedRowStyle(item)?.split(';').filter(Boolean) ?? []
  const customStyle = rowStyle?.(item.row)
  if (customStyle) declarations.push(customStyle)
  if (virtualScrollEnabled && item.region === 'center') {
    declarations.push(`height:${Math.max(1, virtualScrollOptions?.itemSize ?? rowHeight)}px`)
  }
  return declarations.length > 0 ? declarations.join(';') : undefined
}

function sortAriaLabel(column: Column<typeof dataTableFeatures, TData, unknown>): string {
  const next = column.getNextSortingOrder()
  const label = columnLabel(column)
  if (next === 'asc') return resolvedLabels.sortAscending(label)
  if (next === 'desc') return resolvedLabels.sortDescending(label)
  return resolvedLabels.clearSort(label)
}

function updateNumberFilter(column: Column<typeof dataTableFeatures, TData, unknown>, edge: 0 | 1, raw: string): void {
  const current = (column.getFilterValue() as [number | undefined, number | undefined] | undefined) ?? [
    undefined,
    undefined,
  ]
  const next: [number | undefined, number | undefined] = [...current]
  next[edge] = raw === '' ? undefined : Number(raw)
  column.setFilterValue(next[0] === undefined && next[1] === undefined ? undefined : next)
}

function isFilterValueEmpty(value: unknown): boolean {
  if (value == null || value === '') return true
  return Array.isArray(value) && value.every((entry) => entry == null || entry === '')
}

function isFilterValueActive(value: unknown): boolean {
  if (value && typeof value === 'object' && 'constraints' in value) {
    return (value as DataTableFilterGroup).constraints.some((constraint) => !isFilterValueEmpty(constraint.value))
  }
  return !isFilterValueEmpty(value)
}

function textFilterMatchModes(filter: DataTableColumnFilter): readonly DataTableFilterMatchMode[] {
  if (filter.variant !== 'text') return []
  return filter.matchModes ?? ['contains', 'startsWith', 'endsWith', 'equals', 'notEquals']
}

function defaultFilterMatchMode(filter: DataTableColumnFilter): DataTableFilterMatchMode {
  if (filter.variant === 'number-range') return 'between'
  if (filter.variant === 'select') return 'equals'
  return textFilterMatchModes(filter)[0] ?? 'contains'
}

function createFilterGroup(filter: DataTableColumnFilter, value: unknown): DataTableFilterGroup {
  if (value && typeof value === 'object' && 'constraints' in value) {
    const group = value as DataTableFilterGroup
    return {
      operator: group.operator,
      constraints: group.constraints.map((constraint) => ({
        matchMode: constraint.matchMode,
        value: Array.isArray(constraint.value) ? [...constraint.value] : constraint.value,
      })),
    }
  }
  return {
    operator: 'and',
    constraints: [
      {
        matchMode: defaultFilterMatchMode(filter),
        value: filter.variant === 'number-range' ? (Array.isArray(value) ? [...value] : [undefined, undefined]) : value,
      },
    ],
  }
}

function setFilterMenuOpen(
  column: Column<typeof dataTableFeatures, TData, unknown>,
  filter: DataTableColumnFilter,
  open: boolean,
): void {
  if (!open) {
    if (openFilterColumnId === column.id) openFilterColumnId = undefined
    return
  }
  openFilterColumnId = column.id
  filterDraft = createFilterGroup(filter, column.getFilterValue())
}

function updateFilterOperator(operator: DataTableFilterOperator): void {
  if (!filterDraft) return
  filterDraft = { ...filterDraft, operator }
}

function updateFilterConstraint(
  index: number,
  update: Partial<{ value: unknown; matchMode: DataTableFilterMatchMode }>,
): void {
  if (!filterDraft?.constraints[index]) return
  filterDraft = {
    ...filterDraft,
    constraints: filterDraft.constraints.map((constraint, constraintIndex) =>
      constraintIndex === index ? { ...constraint, ...update } : constraint,
    ),
  }
}

function updateDraftNumberFilter(index: number, edge: 0 | 1, raw: string): void {
  const current = filterDraft?.constraints[index]?.value
  const range: [number | undefined, number | undefined] = Array.isArray(current)
    ? [current[0] as number | undefined, current[1] as number | undefined]
    : [undefined, undefined]
  range[edge] = raw === '' ? undefined : Number(raw)
  updateFilterConstraint(index, { value: range })
}

function addFilterConstraint(filter: DataTableColumnFilter): void {
  if (!filterDraft || filter.variant !== 'text') return
  const maximum = Math.max(1, Math.min(filter.maxConstraints ?? 3, 3))
  if (filterDraft.constraints.length >= maximum) return
  filterDraft = {
    ...filterDraft,
    constraints: [
      ...filterDraft.constraints,
      { value: undefined, matchMode: defaultFilterMatchMode(filter) },
    ],
  }
}

function removeFilterConstraint(index: number): void {
  if (!filterDraft || filterDraft.constraints.length === 1) return
  filterDraft = {
    ...filterDraft,
    constraints: filterDraft.constraints.filter((_, constraintIndex) => constraintIndex !== index),
  }
}

function applyColumnFilter(column: Column<typeof dataTableFeatures, TData, unknown>): void {
  const activeConstraints = filterDraft?.constraints.filter((constraint) => !isFilterValueEmpty(constraint.value)) ?? []
  column.setFilterValue(
    activeConstraints.length > 0
      ? {
          operator: filterDraft?.operator ?? 'and',
          constraints: activeConstraints,
        }
      : undefined,
  )
  openFilterColumnId = undefined
}

function clearColumnFilter(column: Column<typeof dataTableFeatures, TData, unknown>): void {
  column.setFilterValue(undefined)
  openFilterColumnId = undefined
}

function clearAllFilters(): void {
  table.setColumnFilters([])
  table.setGlobalFilter('')
  openFilterColumnId = undefined
}

function selectFilterOptions(column: Column<typeof dataTableFeatures, TData, unknown>) {
  const configured = column.columnDef.meta?.filter
  if (configured?.variant === 'select' && configured.options) return configured.options
  return [...column.getFacetedUniqueValues().keys()]
    .filter((value) => value != null)
    .map((value) => ({ value: String(value), label: String(value) }))
    .sort((left, right) => left.label.localeCompare(right.label))
}

function cellEditEvent(cell: DataTableCell<TData>): DataTableCellEditEvent<TData> {
  return {
    cell,
    columnId: cell.column.id,
    row: cell.row,
    original: cell.row.original,
  }
}

function startCellEdit(cell: DataTableCell<TData>): void {
  if (editMode !== 'cell' || !cellEditor || cell.row.getIsGrouped()) return
  editingCell = { rowId: cell.row.id, columnId: cell.column.id }
  onCellEditInit?.(cellEditEvent(cell))
}

function saveCellEdit(cell: DataTableCell<TData>): void {
  onCellEditSave?.(cellEditEvent(cell))
  editingCell = undefined
}

function cancelCellEdit(cell: DataTableCell<TData>): void {
  onCellEditCancel?.(cellEditEvent(cell))
  editingCell = undefined
}

function startRowEdit(row: DataTableRow<TData>): void {
  editingRows = { ...editingRows, [row.id]: true }
  onRowEditInit?.(row)
}

function saveRowEdit(row: DataTableRow<TData>): void {
  const next = { ...editingRows }
  delete next[row.id]
  editingRows = next
  onRowEditSave?.(row)
}

function cancelRowEdit(row: DataTableRow<TData>): void {
  const next = { ...editingRows }
  delete next[row.id]
  editingRows = next
  onRowEditCancel?.(row)
}

function toggleRowSelection(row: DataTableRow<TData>, event?: MouseEvent | KeyboardEvent): void {
  if (!row.getCanSelect()) return
  if (selectionMode === 'single') {
    const next: RowSelectionState = row.getIsSelected() ? {} : { [row.id]: true }
    selectionAnchorId = row.id
    commitRowSelection(next)
    return
  }
  if (selectionMode !== 'multiple') return
  const withMeta = Boolean(event && ('metaKey' in event ? event.metaKey || event.ctrlKey : false))
  const withShift = Boolean(event && 'shiftKey' in event && event.shiftKey)
  if (withShift && selectionAnchorId) {
    selectRowRange(selectionAnchorId, row.id, withMeta)
    return
  } else if (metaKeySelection && !withMeta) {
    commitRowSelection({ [row.id]: true })
  } else {
    const next = { ...rowSelection }
    if (row.getIsSelected()) delete next[row.id]
    else next[row.id] = true
    commitRowSelection(next)
  }
  selectionAnchorId = row.id
}

function selectRowRange(anchorId: string, targetId: string, preserveExisting: boolean): void {
  const rows = renderedRows.map((item) => item.row)
  const anchorIndex = rows.findIndex((row) => row.id === anchorId)
  const targetIndex = rows.findIndex((row) => row.id === targetId)
  if (anchorIndex < 0 || targetIndex < 0) return
  const next: RowSelectionState = preserveExisting ? { ...rowSelection } : {}
  const start = Math.min(anchorIndex, targetIndex)
  const end = Math.max(anchorIndex, targetIndex)
  for (let index = start; index <= end; index += 1) {
    const row = rows[index]
    if (row?.getCanSelect()) next[row.id] = true
  }
  commitRowSelection(next)
}

function handleRowClick(event: MouseEvent, row: DataTableRow<TData>): void {
  onRowClick?.({ event, row, original: row.original })
  if (selectOnRowClick && !isInteractiveTarget(event.target)) toggleRowSelection(row, event)
}

function handleRowContextMenu(event: MouseEvent, row: DataTableRow<TData>): void {
  if (contextMenu) {
    event.preventDefault()
    contextMenuSelection = row.id
    onContextMenuSelectionChange?.(row.original)
  }
  onRowContextMenu?.({ event, row, original: row.original })
}

function handleRowKeydown(event: KeyboardEvent, row: DataTableRow<TData>, index: number): void {
  if (event.target !== event.currentTarget || selectionMode === 'none') return
  if (event.key === ' ' || event.key === 'Enter') {
    event.preventDefault()
    toggleRowSelection(row, event)
    return
  }
  let targetIndex = index
  if (event.key === 'ArrowDown') targetIndex = Math.min(renderedRows.length - 1, index + 1)
  else if (event.key === 'ArrowUp') targetIndex = Math.max(0, index - 1)
  else if (event.key === 'Home') targetIndex = 0
  else if (event.key === 'End') targetIndex = renderedRows.length - 1
  else return
  event.preventDefault()
  const targetRow = renderedRows[targetIndex]?.row
  if (event.shiftKey && targetRow && selectionMode === 'multiple') {
    selectRowRange(selectionAnchorId ?? row.id, targetRow.id, event.metaKey || event.ctrlKey)
  }
  const selector = `[data-data-table-row-index="${targetIndex}"]`
  const targetElement = rootElement.querySelector<HTMLElement>(selector)
  if (targetElement) {
    targetElement.focus()
  } else if (virtualScrollEnabled && viewportElement) {
    const centerIndex = Math.max(0, targetIndex - rowRegions.top.length)
    viewportElement.scrollTop = centerIndex * Math.max(1, virtualScrollOptions?.itemSize ?? rowHeight)
    requestAnimationFrame(() => rootElement.querySelector<HTMLElement>(selector)?.focus())
  }
}

function handleColumnDrop(targetId: string): void {
  if (!draggedColumnId || draggedColumnId === targetId) return
  const order = table.getAllLeafColumns().map((column) => column.id)
  const sourceIndex = order.indexOf(draggedColumnId)
  const targetIndex = order.indexOf(targetId)
  if (sourceIndex < 0 || targetIndex < 0) return
  const [source] = order.splice(sourceIndex, 1)
  order.splice(targetIndex, 0, source)
  table.setColumnOrder(order)
  draggedColumnId = undefined
}

function handleRowDrop(target: DataTableRow<TData>): void {
  if (!draggedRowId || draggedRowId === target.id || !onRowReorder) return
  const rows = renderedRows.map((item) => item.row).filter((row) => !row.getIsGrouped())
  const sourceIndex = rows.findIndex((row) => row.id === draggedRowId)
  const targetIndex = rows.findIndex((row) => row.id === target.id)
  const source = rows[sourceIndex]
  if (!source || sourceIndex < 0 || targetIndex < 0) return
  const reordered = [...rows]
  reordered.splice(sourceIndex, 1)
  reordered.splice(targetIndex, 0, source)
  onRowReorder({ source, target, sourceIndex, targetIndex, rows: reordered.map((row) => row.original) })
  draggedRowId = undefined
}

function resizeColumnByKeyboard(event: KeyboardEvent, column: Column<typeof dataTableFeatures, TData, unknown>): void {
  if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
  event.preventDefault()
  const direction = event.key === 'ArrowRight' ? 1 : -1
  table.setColumnSizing((current) => ({ ...current, [column.id]: column.getSize() + direction * 10 }))
}

function handleViewportScroll(event: Event): void {
  if (!virtualScrollEnabled) return
  const viewport = event.currentTarget as HTMLDivElement
  virtualScrollTop = viewport.scrollTop
  virtualViewportHeight = viewport.clientHeight
}

export function exportCsv(options: DataTableExportOptions<TData> = {}): void {
  exportDataTableCsv({
    table,
    options,
    defaultFilename: exportFilename,
    columnLabel,
    getExportValue,
    onExport,
  })
}

export function getTable(): DataTable<TData> {
  return table
}

export function reset(): void {
  table.reset()
}

onMount(() => {
  let resizeObserver: ResizeObserver | undefined
  if (virtualScrollEnabled && viewportElement) {
    virtualScrollTop = viewportElement.scrollTop
    virtualViewportHeight = viewportElement.clientHeight
    resizeObserver = new ResizeObserver(([entry]) => {
      if (entry) virtualViewportHeight = entry.contentRect.height
    })
    resizeObserver.observe(viewportElement)
  }
  if (stateKey) {
    try {
      const raw = storage().getItem(stateKey)
      if (raw) {
        const state = parseDataTableState(raw, stateKey)
        if (Array.isArray(state.sorting)) sorting = state.sorting
        if (Array.isArray(state.columnFilters)) columnFilters = state.columnFilters
        if (typeof state.globalFilter === 'string') globalFilter = state.globalFilter
        if (
          state.pagination &&
          Number.isInteger(state.pagination.pageIndex) &&
          Number.isInteger(state.pagination.pageSize)
        ) {
          pagination = state.pagination
        }
        if (state.rowSelection && typeof state.rowSelection === 'object') rowSelection = state.rowSelection
        if (state.expanded === true || (state.expanded && typeof state.expanded === 'object')) expanded = state.expanded
        if (Array.isArray(state.grouping)) grouping = state.grouping
        if (state.columnVisibility && typeof state.columnVisibility === 'object')
          columnVisibility = state.columnVisibility
        if (Array.isArray(state.columnOrder)) columnOrder = state.columnOrder
        if (state.columnPinning && typeof state.columnPinning === 'object') columnPinning = state.columnPinning
        if (state.columnSizing && typeof state.columnSizing === 'object') columnSizing = state.columnSizing
        if (state.rowPinning && typeof state.rowPinning === 'object') rowPinning = state.rowPinning
        onStateRestore?.(currentState())
        notifyStateChange()
      }
    } catch (error) {
      reportStateError(error)
    }
  }
  persistenceReady = true
  return () => resizeObserver?.disconnect()
})

$effect(() => {
  const state = currentState()
  if (!persistenceReady || !stateKey) return
  try {
    storage().setItem(stateKey, serializeDataTableState(state))
    onStateSave?.(state)
  } catch (error) {
    reportStateError(error)
  }
})
</script>

{#snippet dataRow(item: RenderedRow, rowIndex: number)}
  <Table.Row
    class={cn(
      'border-border/50',
      stripedRows && 'even:bg-muted/30',
      contextMenuSelection === item.row.id && 'bg-muted',
      (selectionMode !== 'none' || onRowClick) && 'cursor-pointer',
      reorderableRows && !item.row.getIsGrouped() && 'group/data-row',
      rowClass?.(item.row),
    )}
    style={renderedRowStyle(item)}
    data-state={item.row.getIsSelected() ? 'selected' : undefined}
    data-context-menu-selected={contextMenuSelection === item.row.id ? '' : undefined}
    data-data-table-row-index={rowIndex}
    aria-selected={selectionMode === 'none' ? undefined : item.row.getIsSelected()}
    tabindex={selectionMode === 'none' ? undefined : 0}
    onclick={(event) => handleRowClick(event, item.row)}
    ondblclick={(event) => onRowDoubleClick?.({ event, row: item.row, original: item.row.original })}
    oncontextmenu={(event) => handleRowContextMenu(event, item.row)}
    onkeydown={(event) => handleRowKeydown(event, item.row, rowIndex)}
    ondragover={reorderableRows ? (event) => event.preventDefault() : undefined}
    ondrop={reorderableRows ? () => handleRowDrop(item.row) : undefined}>
    {#if reorderableRows}
      <Table.Cell class={cn('w-10', sizeClass('cell'), showGridlines && 'border-e')}>
        {#if !item.row.getIsGrouped()}
          <Button
            variant="ghost"
            size="icon-sm"
            draggable="true"
            aria-label={resolvedLabels.reorderRow(rowIndex + 1)}
            ondragstart={() => (draggedRowId = item.row.id)}
            onclick={(event) => event.stopPropagation()}>
            <GripVerticalIcon />
          </Button>
        {/if}
      </Table.Cell>
    {/if}
    {#if hasSelectionControl}
      <Table.Cell class={cn('w-10', sizeClass('cell'), showGridlines && 'border-e')}>
        <Checkbox
          disabled={!item.row.getCanSelect()}
          aria-label={resolvedLabels.selectRow(rowIndex + 1)}
          bind:checked={() => item.row.getIsSelected(), (value) => item.row.toggleSelected(Boolean(value))}
          onclick={(event) => event.stopPropagation()} />
      </Table.Cell>
    {/if}
    {#if hasExpansionControl}
      <Table.Cell class={cn('w-10', sizeClass('cell'), showGridlines && 'border-e')}>
        {#if item.row.getCanExpand()}
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={item.row.getIsExpanded()
              ? resolvedLabels.collapseRow(rowIndex + 1)
              : resolvedLabels.expandRow(rowIndex + 1)}
            onclick={(event) => {
              event.stopPropagation()
              item.row.toggleExpanded()
            }}>
            {#if item.row.getIsExpanded()}<ChevronUpIcon />{:else}<ChevronDownIcon />{/if}
          </Button>
        {/if}
      </Table.Cell>
    {/if}
    {#if hasEditControl}
      <Table.Cell class={cn('w-20 p-0', showGridlines && 'border-e')}>
        <div class="flex items-center justify-center">
          {#if editingRows[item.row.id]}
            <Button
              variant="ghost"
              size="icon"
              class="size-10"
              aria-label={resolvedLabels.saveRow(rowIndex + 1)}
              onclick={(event) => {
                event.stopPropagation()
                saveRowEdit(item.row)
              }}>
              <CheckIcon class="size-4" />
            </Button>
            <Button
              variant="ghost"
              size="icon"
              class="size-10"
              aria-label={resolvedLabels.cancelRowEdit(rowIndex + 1)}
              onclick={(event) => {
                event.stopPropagation()
                cancelRowEdit(item.row)
              }}>
              <XIcon class="size-4" />
            </Button>
          {:else}
            <Button
              variant="ghost"
              size="icon"
              class="size-10"
              aria-label={resolvedLabels.editRow(rowIndex + 1)}
              onclick={(event) => {
                event.stopPropagation()
                startRowEdit(item.row)
              }}>
              <PencilIcon class="size-4" />
            </Button>
          {/if}
        </div>
      </Table.Cell>
    {/if}
    {#each item.row.getVisibleCells() as cell (cell.id)}
      {#if !cell.getIsCovered()}
        <Table.Cell
          rowspan={cell.getRowSpan()}
          colspan={cell.getColSpan()}
          style={columnInlineStyle(cell.column)}
          class={cn(
            sizeClass('cell'),
            showGridlines && 'border-e last:border-e-0',
            alignClass(cell.column),
            cell.column.columnDef.meta?.cellClass,
            cellClass?.(cell),
          )}
          tabindex={editMode === 'cell' && cellEditor && !cell.row.getIsGrouped() ? 0 : undefined}
          aria-label={editMode === 'cell' && cellEditor && !cell.row.getIsGrouped()
            ? resolvedLabels.editCell(columnLabel(cell.column), rowIndex + 1)
            : undefined}
          ondblclick={editMode === 'cell' && cellEditor
            ? (event) => {
                event.stopPropagation()
                startCellEdit(cell)
              }
            : undefined}
          onkeydown={editMode === 'cell' && cellEditor
            ? (event) => {
                if (event.target === event.currentTarget && event.key === 'Enter') {
                  event.preventDefault()
                  startCellEdit(cell)
                }
              }
            : undefined}>
          {#if cellEditor &&
          !cell.getIsGrouped() &&
          ((editMode === 'cell' && editingCell?.rowId === item.row.id && editingCell.columnId === cell.column.id) ||
            (editMode === 'row' && editingRows[item.row.id]))}
            {@render cellEditor(
              cell,
              editMode === 'cell' ? () => saveCellEdit(cell) : () => saveRowEdit(item.row),
              editMode === 'cell' ? () => cancelCellEdit(cell) : () => cancelRowEdit(item.row),
            )}
          {:else if cell.getIsGrouped()}
            <div class="flex items-center gap-2">
              <Button
                variant="ghost"
                size="icon-sm"
                aria-label={item.row.getIsExpanded()
                  ? resolvedLabels.collapseRow(rowIndex + 1)
                  : resolvedLabels.expandRow(rowIndex + 1)}
                onclick={(event) => {
                  event.stopPropagation()
                  item.row.toggleExpanded()
                }}>
                {#if item.row.getIsExpanded()}<ChevronUpIcon />{:else}<ChevronDownIcon />{/if}
              </Button>
              <FlexRender {cell} />
              <span class="text-xs text-muted-foreground">({item.row.subRows.length})</span>
            </div>
          {:else}
            <FlexRender {cell} />
          {/if}
        </Table.Cell>
      {/if}
    {/each}
  </Table.Row>
  {#if expandedContent && item.row.getIsExpanded() && !item.row.getIsGrouped()}
    <Table.Row class="hover:bg-transparent">
      <Table.Cell colspan={renderedColumnCount} class={cn('whitespace-normal bg-muted/20', sizeClass('cell'))}>
        {@render expandedContent(item.row)}
      </Table.Cell>
    </Table.Row>
  {/if}
{/snippet}

{#snippet paginatorControls()}
  <div class="flex flex-wrap items-center justify-end gap-3" data-slot="data-table-paginator">
    <div class="flex items-center gap-2">
      <span class="text-sm text-muted-foreground">{resolvedLabels.rowsPerPage}</span>
      <Select.Root
        type="single"
        bind:value={() => String(pagination.pageSize), (value) => table.setPageSize(Number(value))}>
        <Select.Trigger class="h-10 w-20">{pagination.pageSize}</Select.Trigger>
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
    <nav class="flex items-center gap-1" aria-label={resolvedLabels.pageStatus(pagination.pageIndex + 1, pageCount)}>
      <Button
        variant="outline"
        size="icon"
        class="size-10"
        aria-label={resolvedLabels.firstPage}
        disabled={!table.getCanPreviousPage()}
        onclick={() => table.firstPage()}>
        <ArrowLeftToLineIcon />
      </Button>
      <Button
        variant="outline"
        size="icon"
        class="size-10"
        aria-label={resolvedLabels.previousPage}
        disabled={!table.getCanPreviousPage()}
        onclick={() => table.previousPage()}>
        <ChevronLeftIcon />
      </Button>
      {#each visiblePageIndexes as pageIndex (pageIndex)}
        <Button
          variant={pageIndex === pagination.pageIndex ? 'default' : 'outline'}
          size="icon"
          class="size-10 tabular-nums"
          aria-label={resolvedLabels.pageStatus(pageIndex + 1, pageCount)}
          aria-current={pageIndex === pagination.pageIndex ? 'page' : undefined}
          onclick={() => table.setPageIndex(pageIndex)}>
          {pageIndex + 1}
        </Button>
      {/each}
      <Button
        variant="outline"
        size="icon"
        class="size-10"
        aria-label={resolvedLabels.nextPage}
        disabled={!table.getCanNextPage()}
        onclick={() => table.nextPage()}>
        <ChevronRightIcon />
      </Button>
      <Button
        variant="outline"
        size="icon"
        class="size-10"
        aria-label={resolvedLabels.lastPage}
        disabled={!table.getCanNextPage()}
        onclick={() => table.lastPage()}>
        <ArrowRightToLineIcon />
      </Button>
    </nav>
  </div>
{/snippet}

<div
  bind:this={rootElement}
  data-slot="data-table"
  class={cn(
    'flex min-w-0 flex-col gap-3',
    scrollHeight &&
      !virtualScrollEnabled &&
      '[&_[data-slot=table-container]]:max-h-[var(--data-table-scroll-height)] [&_[data-slot=table-container]]:overflow-auto',
    className,
  )}
  style:--data-table-scroll-height={scrollHeight}>
  {#if showToolbar}
    <div class="flex flex-wrap items-center gap-2" data-slot="data-table-toolbar">
      {#if toolbar}{@render toolbar(table)}{/if}
      {#if filterDisplay !== 'none' && hasConfiguredColumnFilters}
        <Button variant="outline" class="h-10" disabled={activeFilterCount === 0} onclick={clearAllFilters}>
          <FunnelXIcon data-icon="inline-start" />
          {resolvedLabels.clearFilters}
          {#if activeFilterCount > 0}
            <span class="font-technical tabular-nums">· {activeFilterCount}</span>
          {/if}
        </Button>
      {/if}
      <div class="ms-auto flex min-w-0 flex-1 flex-wrap items-center justify-end gap-2">
        {#if globalFilterEnabled}
          <InputGroup.Root class="w-full min-w-48 sm:w-80">
            <InputGroup.Input
              id={globalFilterId}
              value={globalFilter}
              aria-label={globalFilterPlaceholder ?? resolvedLabels.search}
              placeholder={globalFilterPlaceholder ?? resolvedLabels.search}
              oninput={(event) => table.setGlobalFilter(event.currentTarget.value)} />
            <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
          </InputGroup.Root>
        {/if}
        {#if toolbarEnd}{@render toolbarEnd(table)}{/if}
        {#if columnToggle}
          <ColumnMenu {table} label={resolvedLabels.columns} {columnLabel} />
        {/if}
        {#if exportable}
          <Button
            variant="outline"
            size="sm"
            disabled={table.getPrePaginatedRowModel().flatRows.length === 0}
            onclick={() => exportCsv()}>
            <DownloadIcon data-icon="inline-start" />{resolvedLabels.exportCsv}
          </Button>
        {/if}
      </div>
    </div>
  {/if}
  {#if paginator && (paginatorPosition === 'top' || paginatorPosition === 'both')}
    {@render paginatorControls()}
  {/if}

  <div
    bind:this={viewportElement}
    class={cn(
      'relative min-w-0 overflow-hidden rounded-md border border-border/60',
      virtualScrollEnabled && 'overflow-auto',
    )}
    data-slot="data-table-viewport"
    style:max-height={virtualScrollEnabled ? scrollHeight : undefined}
    onscroll={handleViewportScroll}>
    <Table.Root
      aria-label={ariaLabel}
      aria-busy={loading}
      class={cn(
        showGridlines &&
          'border-collapse [&_[data-slot=table-cell]]:border-border/50 [&_[data-slot=table-head]]:border-border/50',
        tableClass,
      )}>
      {#if caption}<Table.Caption>{caption}</Table.Caption>{/if}
      <Table.Header>
        {#each headerGroups as headerGroup, headerRowIndex (headerGroup.id)}
          <Table.Row class="border-border/50 hover:bg-transparent">
            {#if headerRowIndex === 0}
              {#if reorderableRows}
                <Table.Head rowspan={controlRowSpan} class={cn('w-10', sizeClass('head'), showGridlines && 'border-e')}>
                  <span class="sr-only">{resolvedLabels.reorderRow(0)}</span>
                </Table.Head>
              {/if}
              {#if hasSelectionControl}
                <Table.Head rowspan={controlRowSpan} class={cn('w-10', sizeClass('head'), showGridlines && 'border-e')}>
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
                  class={cn('w-10', sizeClass('head'), showGridlines && 'border-e')} />
              {/if}
              {#if hasEditControl}
                <Table.Head
                  rowspan={controlRowSpan}
                  class={cn('w-20', sizeClass('head'), showGridlines && 'border-e')}>
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
                aria-label={reorderableColumns ? resolvedLabels.reorderColumn(columnLabel(header.column)) : undefined}
                style={headerInlineStyle(header.column, headerRowIndex)}
                class={cn(
                  'relative',
                  sizeClass('head'),
                  showGridlines && 'border-e last:border-e-0',
                  alignClass(header.column),
                  header.column.columnDef.meta?.headerClass,
                  reorderableColumns && header.column.columns.length === 0 && 'cursor-grab active:cursor-grabbing',
                )}
                ondragstart={reorderableColumns ? () => (draggedColumnId = header.column.id) : undefined}
                ondragover={reorderableColumns ? (event) => event.preventDefault() : undefined}
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
                        class={cn('-mx-2 min-w-0', header.column.columnDef.meta?.align === 'end' && 'ms-auto')}
                        aria-label={sortAriaLabel(header.column)}
                        onclick={header.column.getToggleSortingHandler()}>
                        <FlexRender {header} />
                        {#if header.column.getIsSorted() === 'asc'}
                          <ArrowUpIcon data-icon="inline-end" />
                        {:else if header.column.getIsSorted() === 'desc'}
                          <ArrowDownIcon data-icon="inline-end" />
                        {:else}
                          <ArrowUpDownIcon data-icon="inline-end" />
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
                        columnName={columnLabel(header.column)}
                        open={openFilterColumnId === header.column.id}
                        {allFilterValue}
                        selectOptions={selectFilterOptions(header.column)}
                        textMatchModes={textFilterMatchModes(filter)}
                        onOpenChange={(open) => setFilterMenuOpen(header.column, filter, open)}
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
                      aria-label={resolvedLabels.resizeColumn(columnLabel(header.column))}
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
                style={headerInlineStyle(column, headerGroups.length)}
                class={cn(sizeClass('head'), showGridlines && 'border-e last:border-e-0')}>
                {#if filter?.variant === 'text'}
                  <Input
                    class="h-8 min-w-28"
                    value={String(column.getFilterValue() ?? '')}
                    placeholder={filter.placeholder ?? columnLabel(column)}
                    aria-label={filter.placeholder ?? columnLabel(column)}
                    oninput={(event) => column.setFilterValue(event.currentTarget.value || undefined)} />
                {:else if filter?.variant === 'select'}
                  <Select.Root
                    type="single"
                    bind:value={
                      () => String(column.getFilterValue() ?? allFilterValue),
                      (value) => column.setFilterValue(value === allFilterValue ? undefined : value)
                    }>
                    <Select.Trigger class="h-8 min-w-28">
                      {filter.options?.find((option) => option.value === column.getFilterValue())?.label ??
                        (column.getFilterValue() == null
                          ? (filter.allLabel ?? resolvedLabels.allValues)
                          : String(column.getFilterValue()))}
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
                  {@const range =
                    (column.getFilterValue() as [number | undefined, number | undefined] | undefined) ?? []}
                  <div class="flex min-w-48 gap-1">
                    <Input
                      class="h-8 min-w-20"
                      type="number"
                      value={range[0] ?? ''}
                      placeholder={filter.minPlaceholder ?? resolvedLabels.minimum}
                      aria-label={filter.minPlaceholder ?? resolvedLabels.minimum}
                      oninput={(event) => updateNumberFilter(column, 0, event.currentTarget.value)} />
                    <Input
                      class="h-8 min-w-20"
                      type="number"
                      value={range[1] ?? ''}
                      placeholder={filter.maxPlaceholder ?? resolvedLabels.maximum}
                      aria-label={filter.maxPlaceholder ?? resolvedLabels.maximum}
                      oninput={(event) => updateNumberFilter(column, 1, event.currentTarget.value)} />
                  </div>
                {/if}
              </Table.Head>
            {/each}
          </Table.Row>
        {/if}
      </Table.Header>
      <Table.Body>
        {#if loading && renderedRows.length === 0}
          {#each skeletonKeys as key (key)}
            <Table.Row class="hover:bg-transparent">
              {#each Array(renderedColumnCount) as _, columnIndex (`${key}-column-${columnIndex}`)}
                <Table.Cell class={sizeClass('cell')}><Skeleton class="h-5 w-full" /></Table.Cell>
              {/each}
            </Table.Row>
          {/each}
        {:else if renderedRows.length > 0}
          {#each rowRegions.top as item (`top:${item.row.id}`)}
            {@render dataRow(item, item.rowIndex)}
          {/each}
          {#if virtualTopPadding > 0}
            <Table.Row class="border-0 hover:bg-transparent" aria-hidden="true">
              <Table.Cell colspan={renderedColumnCount} class="p-0" style={`height:${virtualTopPadding}px`} />
            </Table.Row>
          {/if}
          {#each visibleCenterRows as item (`center:${item.row.id}`)}
            {@render dataRow(item, item.rowIndex)}
          {/each}
          {#if virtualBottomPadding > 0}
            <Table.Row class="border-0 hover:bg-transparent" aria-hidden="true">
              <Table.Cell colspan={renderedColumnCount} class="p-0" style={`height:${virtualBottomPadding}px`} />
            </Table.Row>
          {/if}
          {#each rowRegions.bottom as item (`bottom:${item.row.id}`)}
            {@render dataRow(item, item.rowIndex)}
          {/each}
        {:else}
          <Table.Row class="hover:bg-transparent">
            <Table.Cell colspan={renderedColumnCount} class="h-32 whitespace-normal p-0 text-center">
              {#if empty}
                {@render empty()}
              {:else}
                <Empty.Root class="py-8">
                  <Empty.Header><Empty.Title>{resolvedLabels.noResults}</Empty.Title></Empty.Header>
                </Empty.Root>
              {/if}
            </Table.Cell>
          </Table.Row>
        {/if}
      </Table.Body>
      {#if showColumnFooters}
        <Table.Footer>
          {#each table.getFooterGroups() as footerGroup (footerGroup.id)}
            <Table.Row>
              {#each Array(controlColumnCount) as _, index (`footer-control-${footerGroup.id}-${index}`)}
                <Table.Cell />
              {/each}
              {#each footerGroup.headers as footerHeader (footerHeader.id)}
                <Table.Cell colspan={footerHeader.colSpan} style={columnInlineStyle(footerHeader.column)}>
                  {#if !footerHeader.isPlaceholder}<FlexRender footer={footerHeader} />{/if}
                </Table.Cell>
              {/each}
            </Table.Row>
          {/each}
        </Table.Footer>
      {/if}
    </Table.Root>
    {#if loading}
      <div
        class="absolute inset-0 z-40 grid place-items-center bg-background/70"
        data-slot="data-table-loading-mask"
        role="status"
        aria-live="polite"
        aria-label={resolvedLabels.loading}>
        {#if loadingContent}
          {@render loadingContent()}
        {:else}
          <div class="flex items-center gap-2 rounded-md border bg-background px-3 py-2 text-sm shadow-sm">
            <Spinner />{resolvedLabels.loading}
          </div>
        {/if}
      </div>
    {/if}
  </div>

  {#if (paginator && (paginatorPosition === 'bottom' || paginatorPosition === 'both')) ||
    selectionMode === 'multiple' ||
    footer}
    <div class="flex flex-wrap items-center gap-3" data-slot="data-table-footer">
      {#if selectionMode === 'multiple'}
        <p class="me-auto text-sm text-muted-foreground">
          {resolvedLabels.selectedRows(
            table.getFilteredSelectedRowModel().flatRows.length,
            table.getFilteredRowModel().flatRows.length,
          )}
        </p>
      {:else}
        <div class="me-auto"></div>
      {/if}
      {#if footer}{@render footer(table)}{/if}
      {#if paginator && (paginatorPosition === 'bottom' || paginatorPosition === 'both')}
        {@render paginatorControls()}
      {/if}
    </div>
  {/if}
</div>
