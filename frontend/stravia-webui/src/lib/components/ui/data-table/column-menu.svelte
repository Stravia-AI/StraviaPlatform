<script lang="ts" generics="TData extends RowData">
import type { Column, RowData } from '@tanstack/svelte-table'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import Columns3Icon from '@lucide/svelte/icons/columns-3'

import { Button } from '$lib/components/ui/button'
import * as DropdownMenu from '$lib/components/ui/dropdown-menu'
import {
  dataTableFeatures,
  type DataTable,
} from './data-table.js'

interface Props {
  table: DataTable<TData>
  label: string
  columnLabel: (column: Column<typeof dataTableFeatures, TData, unknown>) => string
}

let { table, label, columnLabel }: Props = $props()
</script>

<DropdownMenu.Root>
  <DropdownMenu.Trigger>
    {#snippet child({ props })}
      <Button {...props} variant="outline" size="sm">
        <Columns3Icon data-icon="inline-start" />{label}<ChevronDownIcon data-icon="inline-end" />
      </Button>
    {/snippet}
  </DropdownMenu.Trigger>
  <DropdownMenu.Content align="end" class="min-w-44">
    <DropdownMenu.Group>
      <DropdownMenu.Label>{label}</DropdownMenu.Label>
      {#each table.getAllLeafColumns().filter((column) => column.getCanHide()) as column (column.id)}
        <DropdownMenu.CheckboxItem
          bind:checked={() => column.getIsVisible(), (value) => column.toggleVisibility(Boolean(value))}>
          {columnLabel(column)}
        </DropdownMenu.CheckboxItem>
      {/each}
    </DropdownMenu.Group>
  </DropdownMenu.Content>
</DropdownMenu.Root>
