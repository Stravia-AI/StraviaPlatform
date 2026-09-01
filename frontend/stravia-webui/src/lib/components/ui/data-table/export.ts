import type { Column, RowData } from '@tanstack/svelte-table'

import {
  dataTableFeatures,
  type DataTable,
  type DataTableExportOptions,
} from './data-table.js'

interface ExportDataTableCsvInput<TData extends RowData> {
  table: DataTable<TData>
  options: DataTableExportOptions<TData>
  defaultFilename: string
  columnLabel: (column: Column<typeof dataTableFeatures, TData, unknown>) => string
  getExportValue?: (row: TData, columnId: string, value: unknown) => unknown
  onExport?: (event: { filename: string; rowCount: number }) => void
}

function csvCell(value: unknown): string {
  const text = String(value ?? '')
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text
}

export function exportDataTableCsv<TData extends RowData>({
  table,
  options,
  defaultFilename,
  columnLabel,
  getExportValue,
  onExport,
}: ExportDataTableCsvInput<TData>): void {
  const exportColumns = table
    .getVisibleLeafColumns()
    .filter((column) => Boolean(column.accessorFn) && column.columnDef.meta?.exportable !== false)
  const model = options.selectionOnly
    ? table.getSelectedRowModel()
    : options.currentPageOnly
      ? table.getRowModel()
      : table.getPrePaginatedRowModel()
  const rows = model.flatRows.filter((row) => !row.getIsGrouped())
  const header = exportColumns
    .map((column) => {
      const exportHeader = column.columnDef.meta?.exportHeader
      return csvCell(typeof exportHeader === 'function' ? exportHeader() : (exportHeader ?? columnLabel(column)))
    })
    .join(',')
  const lines = rows.map((row) =>
    exportColumns
      .map((column) => {
        const value = row.getValue(column.id)
        return csvCell(options.getValue?.(row.original, column.id, value) ?? getExportValue?.(row.original, column.id, value) ?? value)
      })
      .join(','),
  )
  const blob = new Blob([`\ufeff${[header, ...lines].join('\r\n')}`], { type: 'text/csv;charset=utf-8' })
  const href = URL.createObjectURL(blob)
  const requestedFilename = options.filename ?? defaultFilename
  const filename = requestedFilename.toLowerCase().endsWith('.csv') ? requestedFilename : `${requestedFilename}.csv`
  const anchor = document.createElement('a')
  anchor.href = href
  anchor.download = filename
  anchor.click()
  URL.revokeObjectURL(href)
  onExport?.({ filename, rowCount: rows.length })
}
