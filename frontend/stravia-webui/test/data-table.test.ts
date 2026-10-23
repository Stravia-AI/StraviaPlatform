import { describe, expect, test } from 'bun:test'

import { dataTableCellText, dataTableColumnWidthDeclarations } from '../src/lib/components/ui/data-table/data-table'

describe('data table column width semantics', () => {
  test('fill columns absorb leftover width and keep their size as a floor', () => {
    expect(dataTableColumnWidthDeclarations({ width: 'fill', size: 280, explicit: false, resizable: false })).toEqual([
      'min-width:280px',
    ])
  })

  test('compact columns shrink to content and keep their size as a floor', () => {
    expect(dataTableColumnWidthDeclarations({ width: 'compact', size: 90, explicit: false, resizable: false })).toEqual(
      ['width:1%', 'min-width:90px'],
    )
  })

  test('semantic widths also apply when the table is resizable', () => {
    expect(dataTableColumnWidthDeclarations({ width: 'fill', size: 280, explicit: false, resizable: true })).toEqual([
      'min-width:280px',
    ])
    expect(dataTableColumnWidthDeclarations({ width: 'compact', size: 64, explicit: false, resizable: true })).toEqual([
      'width:1%',
      'min-width:64px',
    ])
  })

  test('pinned or user-resized columns keep the exact width regardless of the semantic', () => {
    expect(dataTableColumnWidthDeclarations({ width: 'compact', size: 200, explicit: true, resizable: true })).toEqual([
      'width:200px',
    ])
    expect(dataTableColumnWidthDeclarations({ width: 'fill', size: 400, explicit: true, resizable: false })).toEqual([
      'width:400px',
    ])
  })

  test('columns without a semantic keep the legacy sizing', () => {
    expect(dataTableColumnWidthDeclarations({ size: 150, explicit: false, resizable: false })).toEqual([])
    expect(dataTableColumnWidthDeclarations({ size: 150, explicit: false, resizable: true })).toEqual(['width:150px'])
  })
})

describe('data table cell text', () => {
  test('renders empty, primitive and structured values for filtering and export', () => {
    expect(dataTableCellText(null)).toBe('')
    expect(dataTableCellText(undefined)).toBe('')
    expect(dataTableCellText('name')).toBe('name')
    expect(dataTableCellText(12)).toBe('12')
    expect(dataTableCellText(false)).toBe('false')
    expect(dataTableCellText({ a: 1 })).toBe('{"a":1}')
  })
})
