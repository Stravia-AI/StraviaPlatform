<script lang="ts" generics="TData extends RowData">
import { FlexRender, type RowData, type RowSelectionState } from '@tanstack/svelte-table'
import type { Snippet } from 'svelte'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import ChevronUpIcon from '@lucide/svelte/icons/chevron-up'
import CheckIcon from '@lucide/svelte/icons/check'
import GripVerticalIcon from '@lucide/svelte/icons/grip-vertical'
import PencilIcon from '@lucide/svelte/icons/pencil'
import XIcon from '@lucide/svelte/icons/x'

import { cn } from '$lib/utils.js'
import { Button } from '$lib/components/ui/button'
import { Checkbox } from '$lib/components/ui/checkbox'
import * as Table from '$lib/components/ui/table'
import type {
  DataTableCell,
  DataTableEditMode,
  DataTableEditingCell,
  DataTableLabels,
  DataTableRow,
  DataTableRowPointerEvent,
  DataTableSelectionMode,
} from './data-table.js'
import type { DataTableRenderStyles, RenderedRow } from './render-types.js'

interface Props {
  item: RenderedRow<TData>
  rowIndex: number
  styles: DataTableRenderStyles<TData>
  resolvedLabels: DataTableLabels
  stripedRows: boolean
  contextMenuSelection: string
  selectionMode: DataTableSelectionMode
  clickable: boolean
  reorderableRows: boolean
  hasSelectionControl: boolean
  hasExpansionControl: boolean
  hasEditControl: boolean
  showGridlines: boolean
  editMode: DataTableEditMode
  editingRows: RowSelectionState
  editingCell: DataTableEditingCell | undefined
  renderedColumnCount: number
  cellEditor: Snippet<[DataTableCell<TData>, () => void, () => void]> | undefined
  expandedContent: Snippet<[DataTableRow<TData>]> | undefined
  groupRow: Snippet<[DataTableRow<TData>]> | undefined
  rowClass: ((row: DataTableRow<TData>) => string | undefined) | undefined
  cellClass: ((cell: DataTableCell<TData>) => string | undefined) | undefined
  onRowDoubleClick: ((event: DataTableRowPointerEvent<TData>) => void) | undefined
  renderedRowStyle: (item: RenderedRow<TData>) => string | undefined
  handleRowClick: (event: MouseEvent, row: DataTableRow<TData>) => void
  handleRowContextMenu: (event: MouseEvent, row: DataTableRow<TData>) => void
  handleRowKeydown: (event: KeyboardEvent, row: DataTableRow<TData>, index: number) => void
  handleRowDrop: (row: DataTableRow<TData>) => void
  startRowDrag: (rowId: string) => void
  startCellEdit: (cell: DataTableCell<TData>) => void
  saveCellEdit: (cell: DataTableCell<TData>) => void
  cancelCellEdit: (cell: DataTableCell<TData>) => void
  startRowEdit: (row: DataTableRow<TData>) => void
  saveRowEdit: (row: DataTableRow<TData>) => void
  cancelRowEdit: (row: DataTableRow<TData>) => void
}

let {
  item,
  rowIndex,
  styles,
  resolvedLabels,
  stripedRows,
  contextMenuSelection,
  selectionMode,
  clickable,
  reorderableRows,
  hasSelectionControl,
  hasExpansionControl,
  hasEditControl,
  showGridlines,
  editMode,
  editingRows,
  editingCell,
  renderedColumnCount,
  cellEditor,
  expandedContent,
  groupRow,
  rowClass,
  cellClass,
  onRowDoubleClick,
  renderedRowStyle,
  handleRowClick,
  handleRowContextMenu,
  handleRowKeydown,
  handleRowDrop,
  startRowDrag,
  startCellEdit,
  saveCellEdit,
  cancelCellEdit,
  startRowEdit,
  saveRowEdit,
  cancelRowEdit,
}: Props = $props()
</script>

{#snippet standardDataRow(item: RenderedRow<TData>, rowIndex: number)}
  <Table.Row
    class={cn(
      'border-border/50',
      stripedRows && 'even:bg-muted/30',
      contextMenuSelection === item.row.id && 'bg-muted',
      (selectionMode !== 'none' || clickable) && 'cursor-pointer',
      reorderableRows && !item.row.getIsGrouped() && 'group/data-row',
      rowClass?.(item.row),
    )}
    style={renderedRowStyle(item)}
    data-state={item.row.getIsSelected() ? 'selected' : undefined}
    data-context-menu-selected={contextMenuSelection === item.row.id ? '' : undefined}
    data-data-table-row-index={rowIndex}
    aria-selected={selectionMode === 'none' ? undefined : item.row.getIsSelected()}
    tabindex={selectionMode === 'none' ? undefined : 0}
    onclick={(event: MouseEvent) => handleRowClick(event, item.row)}
    ondblclick={(event: MouseEvent) => onRowDoubleClick?.({ event, row: item.row, original: item.row.original })}
    oncontextmenu={(event: MouseEvent) => handleRowContextMenu(event, item.row)}
    onkeydown={(event: KeyboardEvent) => handleRowKeydown(event, item.row, rowIndex)}
    ondragover={reorderableRows ? (event: DragEvent) => event.preventDefault() : undefined}
    ondrop={reorderableRows ? () => handleRowDrop(item.row) : undefined}>
    {#if reorderableRows}
      <Table.Cell class={cn('w-10', styles.sizeClass('cell'), showGridlines && 'border-e')}>
        {#if !item.row.getIsGrouped()}
          <Button
            variant="ghost"
            size="icon-sm"
            draggable="true"
            aria-label={resolvedLabels.reorderRow(rowIndex + 1)}
            ondragstart={() => startRowDrag(item.row.id)}
            onclick={(event: MouseEvent) => event.stopPropagation()}>
            <GripVerticalIcon />
          </Button>
        {/if}
      </Table.Cell>
    {/if}
    {#if hasSelectionControl}
      <Table.Cell class={cn('w-10', styles.sizeClass('cell'), showGridlines && 'border-e')}>
        <Checkbox
          disabled={!item.row.getCanSelect()}
          aria-label={resolvedLabels.selectRow(rowIndex + 1)}
          bind:checked={() => item.row.getIsSelected(), (value) => item.row.toggleSelected(Boolean(value))}
          onclick={(event: MouseEvent) => event.stopPropagation()} />
      </Table.Cell>
    {/if}
    {#if hasExpansionControl}
      <Table.Cell class={cn('w-10', styles.sizeClass('cell'), showGridlines && 'border-e')}>
        {#if item.row.getCanExpand()}
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={item.row.getIsExpanded()
              ? resolvedLabels.collapseRow(rowIndex + 1)
              : resolvedLabels.expandRow(rowIndex + 1)}
            onclick={(event: MouseEvent) => {
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
              onclick={(event: MouseEvent) => {
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
              onclick={(event: MouseEvent) => {
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
              onclick={(event: MouseEvent) => {
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
          style={styles.columnInlineStyle(cell.column)}
          class={cn(
            styles.sizeClass('cell'),
            showGridlines && 'border-e last:border-e-0',
            styles.alignClass(cell.column),
            cell.column.columnDef.meta?.cellClass,
            cellClass?.(cell),
          )}
          tabindex={editMode === 'cell' && cellEditor && !cell.row.getIsGrouped() ? 0 : undefined}
          aria-label={editMode === 'cell' && cellEditor && !cell.row.getIsGrouped()
            ? resolvedLabels.editCell(styles.columnLabel(cell.column), rowIndex + 1)
            : undefined}
          ondblclick={editMode === 'cell' && cellEditor
            ? (event: MouseEvent) => {
                event.stopPropagation()
                startCellEdit(cell)
              }
            : undefined}
          onkeydown={editMode === 'cell' && cellEditor
            ? (event: KeyboardEvent) => {
                if (event.target === event.currentTarget && event.key === 'Enter') {
                  event.preventDefault()
                  startCellEdit(cell)
                }
              }
            : undefined}>
          {#if cellEditor && !cell.getIsGrouped() && ((editMode === 'cell' && editingCell?.rowId === item.row.id && editingCell.columnId === cell.column.id) || (editMode === 'row' && editingRows[item.row.id]))}
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
                onclick={(event: MouseEvent) => {
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
      <Table.Cell colspan={renderedColumnCount} class={cn('whitespace-normal bg-muted/20', styles.sizeClass('cell'))}>
        {@render expandedContent(item.row)}
      </Table.Cell>
    </Table.Row>
  {/if}
{/snippet}

{#snippet dataRow(item: RenderedRow<TData>, rowIndex: number)}
  {#if groupRow && item.row.getIsGrouped()}
    <Table.Row
      class={cn(
        'border-border/50',
        contextMenuSelection === item.row.id && 'bg-muted',
        (selectionMode !== 'none' || clickable) && 'cursor-pointer',
        rowClass?.(item.row),
      )}
      style={renderedRowStyle(item)}
      data-state={item.row.getIsSelected() ? 'selected' : undefined}
      data-context-menu-selected={contextMenuSelection === item.row.id ? '' : undefined}
      data-data-table-row-index={rowIndex}
      aria-selected={selectionMode === 'none' ? undefined : item.row.getIsSelected()}
      tabindex={selectionMode === 'none' ? undefined : 0}
      onclick={(event: MouseEvent) => handleRowClick(event, item.row)}
      ondblclick={(event: MouseEvent) => onRowDoubleClick?.({ event, row: item.row, original: item.row.original })}
      oncontextmenu={(event: MouseEvent) => handleRowContextMenu(event, item.row)}
      onkeydown={(event: KeyboardEvent) => handleRowKeydown(event, item.row, rowIndex)}>
      <Table.Cell colspan={renderedColumnCount} class="whitespace-normal p-0">
        {@render groupRow(item.row)}
      </Table.Cell>
    </Table.Row>
  {:else}
    {@render standardDataRow(item, rowIndex)}
  {/if}
{/snippet}

{@render dataRow(item, rowIndex)}
