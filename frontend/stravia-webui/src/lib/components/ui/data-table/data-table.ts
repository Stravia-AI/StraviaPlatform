import {
  aggregationFn_count,
  aggregationFn_max,
  aggregationFn_mean,
  aggregationFn_min,
  aggregationFn_sum,
  cellSpanningFeature,
  columnFacetingFeature,
  columnFilteringFeature,
  columnGroupingFeature,
  columnOrderingFeature,
  columnPinningFeature,
  columnResizingFeature,
  columnSizingFeature,
  columnVisibilityFeature,
  createColumnHelper,
  createExpandedRowModel,
  createFacetedMinMaxValues,
  createFacetedRowModel,
  createFacetedUniqueValues,
  createFilteredRowModel,
  createGroupedRowModel,
  createPaginatedRowModel,
  createSortedRowModel,
  filterFn_equalsString,
  filterFn_inNumberRange,
  filterFn_includesString,
  globalFilteringFeature,
  metaHelper,
  rowAggregationFeature,
  rowExpandingFeature,
  rowPaginationFeature,
  rowPinningFeature,
  rowSelectionFeature,
  rowSortingFeature,
  sortFn_alphanumeric,
  sortFn_basic,
  sortFn_datetime,
  sortFn_text,
  tableFeatures,
  type Cell,
  type CellContext,
  type ColumnDef,
  type ColumnFiltersState,
  type ColumnOrderState,
  type ColumnPinningState,
  type ColumnSizingState,
  type ColumnVisibilityState,
  type ExpandedState,
  type GroupingState,
  type PaginationState,
  type Row,
  type RowData,
  type RowPinningState,
  type RowSelectionState,
  type SortingState,
} from '@tanstack/svelte-table'
import type { SvelteTable } from '@tanstack/svelte-table'

export type DataTableSize = 'small' | 'default' | 'large'
export type DataTableSortMode = 'single' | 'multiple'
export type DataTableSelectionMode = 'none' | 'single' | 'multiple'
export type DataTableFilterDisplay = 'none' | 'row' | 'menu'
export type DataTableStateStorage = 'local' | 'session'
export type DataTableAlign = 'start' | 'center' | 'end'
export type DataTablePaginatorPosition = 'top' | 'bottom' | 'both'
export type DataTableEditMode = 'none' | 'cell' | 'row'
export type DataTableFilterOperator = 'and' | 'or'
export type DataTableFilterMatchMode =
  | 'contains'
  | 'startsWith'
  | 'endsWith'
  | 'equals'
  | 'notEquals'
  | 'lessThan'
  | 'lessThanOrEqual'
  | 'greaterThan'
  | 'greaterThanOrEqual'
  | 'between'

export interface DataTableFilterConstraint {
  value: unknown
  matchMode: DataTableFilterMatchMode
}

export interface DataTableFilterGroup {
  operator: DataTableFilterOperator
  constraints: DataTableFilterConstraint[]
}

export interface DataTableVirtualScrollOptions {
  /** Fixed row height in pixels. Variable-height rows are not virtualized. */
  itemSize: number
  /** Extra rows rendered above and below the visible window. */
  overscan?: number
}

export interface DataTableEditingCell {
  rowId: string
  columnId: string
}

export interface DataTableFilterOption {
  value: string
  label: string
}

export type DataTableColumnFilter =
  | {
      variant: 'text'
      placeholder?: string
      matchModes?: readonly DataTableFilterMatchMode[]
      maxConstraints?: number
    }
  | { variant: 'select'; placeholder?: string; allLabel?: string; options?: readonly DataTableFilterOption[] }
  | { variant: 'number-range'; minPlaceholder?: string; maxPlaceholder?: string }

export interface DataTableColumnMeta {
  /** Human-readable label used by column controls and CSV export. */
  label?: string | (() => string)
  align?: DataTableAlign
  headerClass?: string
  cellClass?: string
  filter?: DataTableColumnFilter
  exportable?: boolean
  exportHeader?: string | (() => string)
}

function isEmptyFilterValue(value: unknown): boolean {
  if (value == null || value === '') return true
  return Array.isArray(value) && value.every((entry) => entry == null || entry === '')
}

function matchesFilterConstraint(value: unknown, constraint: DataTableFilterConstraint): boolean {
  const filterValue = constraint.value
  if (isEmptyFilterValue(filterValue)) return true

  if (constraint.matchMode === 'between') {
    const [minimum, maximum] = Array.isArray(filterValue) ? filterValue : []
    const numericValue = Number(value)
    return (
      Number.isFinite(numericValue) &&
      (minimum == null || minimum === '' || numericValue >= Number(minimum)) &&
      (maximum == null || maximum === '' || numericValue <= Number(maximum))
    )
  }

  if (
    constraint.matchMode === 'lessThan' ||
    constraint.matchMode === 'lessThanOrEqual' ||
    constraint.matchMode === 'greaterThan' ||
    constraint.matchMode === 'greaterThanOrEqual'
  ) {
    const numericValue = Number(value)
    const numericFilter = Number(filterValue)
    if (!Number.isFinite(numericValue) || !Number.isFinite(numericFilter)) return false
    if (constraint.matchMode === 'lessThan') return numericValue < numericFilter
    if (constraint.matchMode === 'lessThanOrEqual') return numericValue <= numericFilter
    if (constraint.matchMode === 'greaterThan') return numericValue > numericFilter
    return numericValue >= numericFilter
  }

  const candidate = String(value ?? '').toLocaleLowerCase()
  const query = String(filterValue).toLocaleLowerCase()
  if (constraint.matchMode === 'startsWith') return candidate.startsWith(query)
  if (constraint.matchMode === 'endsWith') return candidate.endsWith(query)
  if (constraint.matchMode === 'equals') return candidate === query
  if (constraint.matchMode === 'notEquals') return candidate !== query
  return candidate.includes(query)
}

function matchesDataTableFilter(value: unknown, filterValue: unknown): boolean {
  if (filterValue && typeof filterValue === 'object' && 'constraints' in filterValue) {
    const group = filterValue as DataTableFilterGroup
    const constraints = group.constraints.filter((constraint) => !isEmptyFilterValue(constraint.value))
    if (constraints.length === 0) return true
    return group.operator === 'or'
      ? constraints.some((constraint) => matchesFilterConstraint(value, constraint))
      : constraints.every((constraint) => matchesFilterConstraint(value, constraint))
  }
  if (Array.isArray(filterValue)) {
    return matchesFilterConstraint(value, { value: filterValue, matchMode: 'between' })
  }
  return matchesFilterConstraint(value, { value: filterValue, matchMode: 'contains' })
}

export const dataTableFeatures = tableFeatures({
  columnFilteringFeature,
  filteredRowModel: createFilteredRowModel(),
  filterFns: {
    dataTable: (row, columnId, filterValue) => matchesDataTableFilter(row.getValue(columnId), filterValue),
    equalsString: filterFn_equalsString,
    includesString: filterFn_includesString,
    inNumberRange: filterFn_inNumberRange,
  },
  globalFilteringFeature,
  columnFacetingFeature,
  facetedRowModel: createFacetedRowModel(),
  facetedUniqueValues: createFacetedUniqueValues(),
  facetedMinMaxValues: createFacetedMinMaxValues(),
  rowAggregationFeature,
  columnGroupingFeature,
  groupedRowModel: createGroupedRowModel(),
  aggregationFns: {
    count: aggregationFn_count,
    max: aggregationFn_max,
    mean: aggregationFn_mean,
    min: aggregationFn_min,
    sum: aggregationFn_sum,
  },
  rowSortingFeature,
  sortedRowModel: createSortedRowModel(),
  sortFns: { alphanumeric: sortFn_alphanumeric, basic: sortFn_basic, datetime: sortFn_datetime, text: sortFn_text },
  rowExpandingFeature,
  expandedRowModel: createExpandedRowModel(),
  rowPaginationFeature,
  paginatedRowModel: createPaginatedRowModel(),
  rowSelectionFeature,
  rowPinningFeature,
  columnVisibilityFeature,
  columnOrderingFeature,
  columnPinningFeature,
  columnSizingFeature,
  columnResizingFeature,
  cellSpanningFeature,
  columnMeta: metaHelper<DataTableColumnMeta>(),
})

export type DataTable<TData extends RowData> = SvelteTable<typeof dataTableFeatures, TData>
export type DataTableColumn<TData extends RowData> = ColumnDef<typeof dataTableFeatures, TData, unknown>
export type DataTableCell<TData extends RowData> = Cell<typeof dataTableFeatures, TData, unknown>
export type DataTableCellContext<TData extends RowData> = CellContext<typeof dataTableFeatures, TData, unknown>
export type DataTableRow<TData extends RowData> = Row<typeof dataTableFeatures, TData>

export function createDataTableColumnHelper<TData extends RowData>() {
  return createColumnHelper<typeof dataTableFeatures, TData>()
}

export interface DataTableState {
  sorting: SortingState
  columnFilters: ColumnFiltersState
  globalFilter: string
  pagination: PaginationState
  rowSelection: RowSelectionState
  expanded: ExpandedState
  grouping: GroupingState
  columnVisibility: ColumnVisibilityState
  columnOrder: ColumnOrderState
  columnPinning: ColumnPinningState
  columnSizing: ColumnSizingState
  rowPinning: RowPinningState
}

export interface DataTableLabels {
  search: string
  columns: string
  exportCsv: string
  noResults: string
  loading: string
  rowsPerPage: string
  selectedRows: (selected: number, total: number) => string
  pageStatus: (page: number, pageCount: number) => string
  firstPage: string
  previousPage: string
  nextPage: string
  lastPage: string
  selectAllRows: string
  selectRow: (index: number) => string
  expandRow: (index: number) => string
  collapseRow: (index: number) => string
  editRow: (index: number) => string
  saveRow: (index: number) => string
  cancelRowEdit: (index: number) => string
  editCell: (column: string, index: number) => string
  saveCell: string
  cancelCellEdit: string
  sortAscending: (column: string) => string
  sortDescending: (column: string) => string
  clearSort: (column: string) => string
  clearFilters: string
  filterBy: (column: string) => string
  showFilterMenu: (column: string) => string
  hideFilterMenu: (column: string) => string
  applyFilter: string
  clearFilter: string
  matchAll: string
  matchAny: string
  matchMode: string
  addFilterRule: string
  removeFilterRule: (index: number) => string
  filterMatchMode: (mode: DataTableFilterMatchMode) => string
  reorderColumn: (column: string) => string
  resizeColumn: (column: string) => string
  reorderRow: (index: number) => string
  allValues: string
  minimum: string
  maximum: string
}

export const defaultDataTableLabels: DataTableLabels = {
  search: 'Search all columns…',
  columns: 'Columns',
  exportCsv: 'Export CSV',
  noResults: 'No results.',
  loading: 'Loading rows…',
  rowsPerPage: 'Rows per page',
  selectedRows: (selected, total) => `${selected} of ${total} row(s) selected`,
  pageStatus: (page, pageCount) => `Page ${page} of ${pageCount}`,
  firstPage: 'Go to first page',
  previousPage: 'Go to previous page',
  nextPage: 'Go to next page',
  lastPage: 'Go to last page',
  selectAllRows: 'Select all rows on this page',
  selectRow: (index) => `Select row ${index}`,
  expandRow: (index) => `Expand row ${index}`,
  collapseRow: (index) => `Collapse row ${index}`,
  editRow: (index) => `Edit row ${index}`,
  saveRow: (index) => `Save row ${index}`,
  cancelRowEdit: (index) => `Cancel editing row ${index}`,
  editCell: (column, index) => `Edit ${column} in row ${index}`,
  saveCell: 'Save cell',
  cancelCellEdit: 'Cancel cell editing',
  sortAscending: (column) => `Sort ${column} ascending`,
  sortDescending: (column) => `Sort ${column} descending`,
  clearSort: (column) => `Clear sorting for ${column}`,
  clearFilters: 'Clear filters',
  filterBy: (column) => `Filter by ${column}`,
  showFilterMenu: (column) => `Show filter menu for ${column}`,
  hideFilterMenu: (column) => `Hide filter menu for ${column}`,
  applyFilter: 'Apply',
  clearFilter: 'Clear',
  matchAll: 'Match all',
  matchAny: 'Match any',
  matchMode: 'Match mode',
  addFilterRule: 'Add rule',
  removeFilterRule: (index) => `Remove filter rule ${index}`,
  filterMatchMode: (mode) =>
    ({
      contains: 'Contains',
      startsWith: 'Starts with',
      endsWith: 'Ends with',
      equals: 'Equals',
      notEquals: 'Does not equal',
      lessThan: 'Less than',
      lessThanOrEqual: 'Less than or equal',
      greaterThan: 'Greater than',
      greaterThanOrEqual: 'Greater than or equal',
      between: 'Between',
    })[mode],
  reorderColumn: (column) => `Drag to reorder ${column} column`,
  resizeColumn: (column) => `Resize ${column} column`,
  reorderRow: (index) => `Drag to reorder row ${index}`,
  allValues: 'All',
  minimum: 'Minimum',
  maximum: 'Maximum',
}

export interface DataTableRowEvent<TData extends RowData> {
  row: DataTableRow<TData>
  original: TData
}

export interface DataTableRowPointerEvent<TData extends RowData> extends DataTableRowEvent<TData> {
  event: MouseEvent
}

export interface DataTableCellEditEvent<TData extends RowData> extends DataTableRowEvent<TData> {
  cell: DataTableCell<TData>
  columnId: string
}

export interface DataTableRowReorderEvent<TData extends RowData> {
  source: DataTableRow<TData>
  target: DataTableRow<TData>
  sourceIndex: number
  targetIndex: number
  rows: TData[]
}

export interface DataTableExportOptions<TData extends RowData> {
  filename?: string
  selectionOnly?: boolean
  currentPageOnly?: boolean
  getValue?: (row: TData, columnId: string, value: unknown) => unknown
}
