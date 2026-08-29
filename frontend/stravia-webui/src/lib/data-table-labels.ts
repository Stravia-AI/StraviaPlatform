import * as m from '$lib/paraglide/messages.js'

import type { DataTableFilterMatchMode, DataTableLabels } from '$lib/components/ui/data-table'

function getFilterMatchModeLabel(mode: DataTableFilterMatchMode): string {
  return {
    between: m.common_table_filter_match_between(),
    contains: m.common_table_filter_match_contains(),
    endsWith: m.common_table_filter_match_ends_with(),
    equals: m.common_table_filter_match_equals(),
    greaterThan: m.common_table_filter_match_greater_than(),
    greaterThanOrEqual: m.common_table_filter_match_greater_than_or_equal(),
    lessThan: m.common_table_filter_match_less_than(),
    lessThanOrEqual: m.common_table_filter_match_less_than_or_equal(),
    notEquals: m.common_table_filter_match_not_equals(),
    startsWith: m.common_table_filter_match_starts_with(),
  }[mode]
}

export function getDataTableLabels(): Partial<DataTableLabels> {
  return {
    addFilterRule: m.common_table_add_filter_rule(),
    applyFilter: m.common_table_apply_filter(),
    cancelCellEdit: m.common_table_cancel_cell_edit(),
    cancelRowEdit: (index) => m.common_table_cancel_row_edit({ index }),
    clearFilter: m.common_table_clear_filter(),
    clearFilters: m.common_table_clear_filters(),
    columns: m.common_table_columns(),
    editCell: (column, index) => m.common_table_edit_cell({ column, index }),
    editRow: (index) => m.common_table_edit_row({ index }),
    exportCsv: m.common_table_export_csv(),
    filterBy: (column) => m.common_table_filter_by({ column }),
    filterMatchMode: getFilterMatchModeLabel,
    hideFilterMenu: (column) => m.common_table_hide_filter_menu({ column }),
    matchAll: m.common_table_match_all(),
    matchAny: m.common_table_match_any(),
    matchMode: m.common_table_match_mode(),
    removeFilterRule: (index) => m.common_table_remove_filter_rule({ index }),
    sortAscending: (column) => m.common_table_sort_ascending({ column }),
    sortDescending: (column) => m.common_table_sort_descending({ column }),
    clearSort: (column) => m.common_table_clear_sort({ column }),
    reorderColumn: (column) => m.common_table_reorder_column({ column }),
    resizeColumn: (column) => m.common_table_resize_column({ column }),
    saveCell: m.common_table_save_cell(),
    saveRow: (index) => m.common_table_save_row({ index }),
    showFilterMenu: (column) => m.common_table_show_filter_menu({ column }),
  }
}
