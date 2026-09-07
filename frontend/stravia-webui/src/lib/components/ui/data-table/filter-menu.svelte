<script lang="ts" generics="TData extends RowData">
import type { Column, RowData } from '@tanstack/svelte-table'
import FunnelIcon from '@lucide/svelte/icons/funnel'
import PlusIcon from '@lucide/svelte/icons/plus'
import XIcon from '@lucide/svelte/icons/x'

import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import * as Popover from '$lib/components/ui/popover'
import * as Select from '$lib/components/ui/select'
import { cn } from '$lib/utils.js'
import {
  dataTableFeatures,
  type DataTableColumnFilter,
  type DataTableFilterConstraint,
  type DataTableFilterGroup,
  type DataTableFilterMatchMode,
  type DataTableFilterOperator,
  type DataTableFilterOption,
  type DataTableLabels,
} from './data-table.js'

interface Props {
  column: Column<typeof dataTableFeatures, TData, unknown>
  filter: DataTableColumnFilter
  draft?: DataTableFilterGroup
  labels: DataTableLabels
  columnName: string
  open: boolean
  allFilterValue: string
  selectOptions: readonly DataTableFilterOption[]
  textMatchModes: readonly DataTableFilterMatchMode[]
  onOpenChange: (open: boolean) => void
  onUpdateOperator: (operator: DataTableFilterOperator) => void
  onUpdateConstraint: (index: number, patch: Partial<DataTableFilterConstraint>) => void
  onAddConstraint: () => void
  onRemoveConstraint: (index: number) => void
  onUpdateNumber: (constraintIndex: number, rangeIndex: 0 | 1, value: string) => void
  onClear: () => void
  onApply: () => void
}

let {
  column,
  filter,
  draft,
  labels,
  columnName,
  open,
  allFilterValue,
  selectOptions,
  textMatchModes,
  onOpenChange,
  onUpdateOperator,
  onUpdateConstraint,
  onAddConstraint,
  onRemoveConstraint,
  onUpdateNumber,
  onClear,
  onApply,
}: Props = $props()
</script>

<Popover.Root bind:open={() => open, onOpenChange}>
  <Popover.Trigger>
    {#snippet child({ props })}
      <Button
        {...props}
        variant="ghost"
        size="icon"
        class={cn('-me-2 size-10 shrink-0', column.getIsFiltered() && 'bg-muted text-primary hover:text-primary')}
        aria-label={open ? labels.hideFilterMenu(columnName) : labels.showFilterMenu(columnName)}
        aria-haspopup="dialog"
        aria-expanded={open}>
        <FunnelIcon class="size-3.5" />
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content
    align={column.columnDef.meta?.align === 'end' ? 'end' : 'start'}
    class="flex max-h-[min(42rem,var(--bits-popover-content-available-height))] w-72 flex-col p-0"
    role="dialog"
    aria-label={labels.filterBy(columnName)}>
    <Popover.Header class="shrink-0 border-b border-border/60 px-4 py-3">
      <Popover.Title>{labels.filterBy(columnName)}</Popover.Title>
    </Popover.Header>
    {#if draft}
      <div class="flex min-h-0 flex-col gap-3 overflow-y-auto p-4">
        {#if filter.variant === 'custom'}
          {@render filter.content(draft.constraints[0]?.value, (value) => onUpdateConstraint(0, { value }))}
        {:else if filter.variant === 'text'}
          {#if draft.constraints.length > 1}
            <Select.Root
              type="single"
              bind:value={() => draft.operator, (value) => onUpdateOperator(value as DataTableFilterOperator)}>
              <Select.Trigger class="h-10 w-full" aria-label={labels.matchMode}>
                {draft.operator === 'and' ? labels.matchAll : labels.matchAny}
              </Select.Trigger>
              <Select.Content>
                <Select.Group>
                  <Select.Item value="and">{labels.matchAll}</Select.Item>
                  <Select.Item value="or">{labels.matchAny}</Select.Item>
                </Select.Group>
              </Select.Content>
            </Select.Root>
          {/if}
          {#each draft.constraints as constraint, constraintIndex (constraint)}
            <div class="flex flex-col gap-2">
              <div class="flex items-center gap-2">
                <Select.Root
                  type="single"
                  bind:value={
                    () => constraint.matchMode,
                    (value) => onUpdateConstraint(constraintIndex, { matchMode: value as DataTableFilterMatchMode })
                  }>
                  <Select.Trigger class="h-10 min-w-0 flex-1" aria-label={labels.matchMode}>
                    {labels.filterMatchMode(constraint.matchMode)}
                  </Select.Trigger>
                  <Select.Content>
                    <Select.Group>
                      {#each textMatchModes as matchMode (matchMode)}
                        <Select.Item value={matchMode}>{labels.filterMatchMode(matchMode)}</Select.Item>
                      {/each}
                    </Select.Group>
                  </Select.Content>
                </Select.Root>
                {#if draft.constraints.length > 1}
                  <Button
                    variant="ghost"
                    size="icon"
                    class="size-10 shrink-0"
                    aria-label={labels.removeFilterRule(constraintIndex + 1)}
                    onclick={() => onRemoveConstraint(constraintIndex)}>
                    <XIcon class="size-4" />
                  </Button>
                {/if}
              </div>
              <Input
                class="h-10"
                value={String(constraint.value ?? '')}
                placeholder={filter.placeholder ?? columnName}
                aria-label={filter.placeholder ?? columnName}
                oninput={(event) =>
                  onUpdateConstraint(constraintIndex, { value: event.currentTarget.value || undefined })} />
            </div>
          {/each}
          {#if draft.constraints.length < Math.max(1, Math.min(filter.maxConstraints ?? 3, 3))}
            <Button variant="ghost" class="h-10 w-full justify-start" onclick={onAddConstraint}>
              <PlusIcon data-icon="inline-start" />{labels.addFilterRule}
            </Button>
          {/if}
        {:else if filter.variant === 'select'}
          <Select.Root
            type="single"
            bind:value={
              () => String(draft.constraints[0]?.value ?? allFilterValue),
              (value) => onUpdateConstraint(0, { value: value === allFilterValue ? undefined : value })
            }>
            <Select.Trigger class="h-10 w-full" aria-label={filter.placeholder ?? columnName}>
              {filter.options?.find((option) => option.value === draft.constraints[0]?.value)?.label ??
                filter.allLabel ??
                labels.allValues}
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
                <Select.Item value={allFilterValue}>{filter.allLabel ?? labels.allValues}</Select.Item>
                {#each selectOptions as option (option.value)}
                  <Select.Item value={option.value}>{option.label}</Select.Item>
                {/each}
              </Select.Group>
            </Select.Content>
          </Select.Root>
        {:else}
          {@const range = (draft.constraints[0]?.value as [number | undefined, number | undefined] | undefined) ?? []}
          <div class="grid grid-cols-2 gap-2">
            <Input
              class="h-10 min-w-0"
              type="number"
              value={range[0] ?? ''}
              placeholder={filter.minPlaceholder ?? labels.minimum}
              aria-label={filter.minPlaceholder ?? labels.minimum}
              oninput={(event) => onUpdateNumber(0, 0, event.currentTarget.value)} />
            <Input
              class="h-10 min-w-0"
              type="number"
              value={range[1] ?? ''}
              placeholder={filter.maxPlaceholder ?? labels.maximum}
              aria-label={filter.maxPlaceholder ?? labels.maximum}
              oninput={(event) => onUpdateNumber(0, 1, event.currentTarget.value)} />
          </div>
        {/if}
      </div>
      <div class="flex shrink-0 items-center justify-between border-t border-border/60 px-4 py-3">
        <Button variant="outline" class="h-10" onclick={onClear}>{labels.clearFilter}</Button>
        <Button class="h-10" onclick={onApply}>{labels.applyFilter}</Button>
      </div>
    {/if}
  </Popover.Content>
</Popover.Root>
