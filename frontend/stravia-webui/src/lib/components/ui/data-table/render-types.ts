import type { Column, RowData } from '@tanstack/svelte-table'

import type { DataTableRow, dataTableFeatures } from './data-table.js'

export type DataTableColumnHandle<TData extends RowData> = Column<typeof dataTableFeatures, TData, unknown>

export interface DataTableRenderStyles<TData extends RowData> {
  columnLabel: (column: DataTableColumnHandle<TData>) => string
  alignClass: (column: DataTableColumnHandle<TData>) => string | undefined
  sizeClass: (section: 'head' | 'cell') => string
  columnInlineStyle: (column: DataTableColumnHandle<TData>, header?: boolean) => string | undefined
}

export interface RenderedRow<TData extends RowData> {
  row: DataTableRow<TData>
  region: 'top' | 'center' | 'bottom'
  regionIndex: number
  regionCount: number
  rowIndex: number
}
