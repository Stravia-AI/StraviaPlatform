<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createInfiniteQuery, createQuery } from '@tanstack/svelte-query'
import { tick } from 'svelte'
import { SvelteMap } from 'svelte/reactivity'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatLogTime } from '#lib/format.js'
import {
  changeTypes,
  changeLabel,
  sourceLabel,
  policyLabel,
  specificationField,
  specificationValue,
  modelChangeReason,
} from '#lib/model-changes.js'
import { specificationRows } from '#lib/model-specification-diff.js'
import type { ModelChangeType, ModelChangeSource, ModelChangeRecord } from '#lib/types.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Table from '#lib/components/ui/table/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import ChevronRightIcon from '@lucide/svelte/icons/chevron-right'
import SearchIcon from '@lucide/svelte/icons/search'
import { cn } from '#lib/utils.js'
let open = $state(false)
let providerId = $state('')
let type = $state<ModelChangeType | ''>('')
let source = $state<ModelChangeSource | ''>('')
let search = $state('')
let selectedId = $state('')
let expanded = $state<Record<string, boolean>>({})
let restoreTarget: HTMLElement | null = null
let selectedElement = $state<HTMLDivElement>()
const providers = createQuery(() => ({ queryKey: ['providers'], queryFn: admin.providers.list, enabled: open }))
const history = createInfiniteQuery(() => ({
  queryKey: ['model-changes', providerId, type, source, search],
  enabled: open,
  refetchInterval: 15_000,
  initialPageParam: undefined as string | undefined,
  queryFn: ({ pageParam }) =>
    admin.modelChanges.list({
      provider_id: providerId || undefined,
      type: type || undefined,
      source: source || undefined,
      q: search || undefined,
      cursor: pageParam,
      limit: 25,
    }),
  getNextPageParam: (last) => last.next_cursor ?? undefined,
}))
const selected = createQuery(() => ({
  queryKey: ['model-change', selectedId],
  queryFn: () => admin.modelChanges.get(selectedId),
  enabled: open && Boolean(selectedId),
}))
const records = $derived(history.data?.pages.flatMap((item) => item.records) ?? [])
const serviceOptions = $derived.by(() => {
  const options = new SvelteMap((providers.data ?? []).map((provider) => [provider.id, provider.name]))
  for (const record of records) {
    if (!options.has(record.provider_id))
      options.set(
        record.provider_id,
        `${record.provider_name}${record.provider_deleted ? ` · ${m.model_changes_deleted()}` : ''}`,
      )
  }
  if (selected.data && !options.has(selected.data.provider_id))
    options.set(
      selected.data.provider_id,
      `${selected.data.provider_name}${selected.data.provider_deleted ? ` · ${m.model_changes_deleted()}` : ''}`,
    )
  return options
})
const filtered = $derived(Boolean(providerId || type || source || search))
function focusFallback(): HTMLElement | null {
  for (const selector of [
    '[data-model-changes-trigger]',
    'main [aria-current="page"]',
    'nav a[aria-current="page"]',
    'header button[aria-expanded]',
  ]) {
    const target = [...document.querySelectorAll<HTMLElement>(selector)].find(
      (element) => element.getClientRects().length > 0,
    )
    if (target) return target
  }
  return null
}
export function show(defaultProvider = ''): void {
  restoreTarget =
    document.activeElement instanceof HTMLElement && document.activeElement !== document.body
      ? document.activeElement
      : focusFallback()
  providerId = defaultProvider
  type = ''
  source = ''
  search = ''
  selectedId = ''
  expanded = {}
  open = true
}
export function reveal(id: string): void {
  const active = document.activeElement
  restoreTarget =
    active instanceof HTMLElement && active !== document.body && !active.closest('[data-sonner-toast]')
      ? active
      : focusFallback()
  selectedId = id
  expanded = { ...expanded, [id]: true }
  open = true
}
$effect(() => {
  if (open && selected.data && selectedElement) {
    const target = selectedElement
    void tick().then(() => {
      target.scrollIntoView({ block: 'nearest' })
      target.focus()
    })
  }
})
</script>

{#snippet batch(record: ModelChangeRecord)}
  {@const open = Boolean(expanded[record.id])}
  {@const updatedModels = record.changes.filter((item) => item.types.includes('specification_updated'))}
  <article class="min-w-0 border-b last:border-b-0">
    <Button
      variant="ghost"
      class="h-auto w-full items-start justify-start gap-3 rounded-md px-2 py-3 text-left font-normal whitespace-normal"
      aria-expanded={open}
      aria-controls={`batch-${record.id}`}
      onclick={() => (expanded[record.id] = !expanded[record.id])}>
      <ChevronRightIcon
        class={cn(
          'mt-0.5 size-4 shrink-0 text-muted-foreground transition-transform duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)]',
          open && 'rotate-90',
        )}
        aria-hidden="true" />
      <span class="flex min-w-0 flex-1 flex-col gap-1.5">
        <span class="flex min-w-0 flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5">
          <span class="max-w-full min-w-0 truncate font-medium">{record.provider_name}</span>
          <time class="shrink-0 font-technical text-xs text-muted-foreground" datetime={record.checked_at}
            >{formatLogTime(record.checked_at)}</time>
        </span>
        <span
          class={cn(
            'flex flex-wrap items-center gap-x-1.5 gap-y-1 text-[0.8125rem] leading-[1.45]',
            record.outcome === 'success' ? 'text-foreground' : 'text-destructive',
          )}>
          <span
            >{record.outcome === 'failure'
              ? m.model_changes_failed()
              : record.outcome === 'partial_success'
                ? m.model_changes_partial()
                : [...new Set(record.changes.flatMap((item) => item.types))]
                    .map(
                      (kind) =>
                        `${changeLabel(kind)} ${record.changes.filter((item) => item.types.includes(kind)).length}`,
                    )
                    .join(' · ')}</span>
          <span class="text-muted-foreground">· {sourceLabel(record.source)}</span>
          {#if record.initial_sync}<Badge variant="outline">{m.model_changes_initial()}</Badge>{/if}
          {#if record.provider_deleted}<Badge variant="outline">{m.model_changes_deleted()}</Badge>{/if}
        </span>
        {#if !open && updatedModels.length > 0}
          <span class="text-xs break-words text-muted-foreground">
            {m.model_changes_specification_updated()}:
            {updatedModels
              .slice(0, 3)
              .map((item) => item.name || item.model_id)
              .join(', ')}
            {#if updatedModels.length > 3}
              <span class="text-muted-foreground">
                {m.model_changes_more_models({ count: updatedModels.length - 3 })}
              </span>
            {/if}
          </span>
        {/if}
      </span>
    </Button>
    {#if open}
      <div id={`batch-${record.id}`} class="ms-[1.125rem] mb-3 flex min-w-0 flex-col border-s ps-4">
        {#if record.error}<p class="py-2 text-[0.8125rem] break-words text-destructive">
            {modelChangeReason(record.error)}
          </p>{/if}
        {#each record.changes as item (item.model_id)}
          {@const differences = item.specification.flatMap(specificationRows)}
          <div class="flex min-w-0 flex-col gap-3 border-b border-border/60 py-3 last:border-b-0">
            <div class="flex min-w-0 flex-col gap-2">
              <div class="min-w-0">
                <h3 id={`model-change-${record.id}-${item.model_id}`} class="text-sm font-semibold break-all">
                  {item.name || item.model_id}
                </h3>
                {#if item.name && item.name !== item.model_id}<p
                    class="font-technical text-xs break-all text-muted-foreground">
                    {item.model_id}
                  </p>{/if}
              </div>
              <div class="flex flex-wrap gap-1">
                {#each item.types as kind (kind)}<Badge variant="outline">{changeLabel(kind)}</Badge>{/each}
              </div>
            </div>
            {#if item.types.includes('selection_policy_changed') && item.before_policy && item.after_policy}<p
                class="text-[0.8125rem]">
                {policyLabel(item.before_policy)} → {policyLabel(item.after_policy)}
              </p>{/if}
            {#if differences.length > 0}
              <Table.Root
                class="table-fixed text-[0.8125rem]"
                aria-labelledby={`model-change-${record.id}-${item.model_id}`}>
                <Table.Header>
                  <Table.Row>
                    <Table.Head scope="col" class="w-[36%] ps-0 whitespace-normal"
                      >{m.model_changes_field()}</Table.Head>
                    <Table.Head scope="col" class="w-[32%] whitespace-normal">{m.model_changes_before()}</Table.Head>
                    <Table.Head scope="col" class="w-[32%] pe-0 whitespace-normal"
                      >{m.model_changes_after()}</Table.Head>
                  </Table.Row>
                </Table.Header>
                <Table.Body>
                  {#each differences as diff (diff.field)}
                    <Table.Row>
                      <Table.Head scope="row" class="h-auto py-2 ps-0 align-top break-words whitespace-normal">
                        {specificationField(diff.field)}
                      </Table.Head>
                      <Table.Cell class="align-top break-words whitespace-normal">
                        <span class={cn('tabular-nums', typeof diff.before === 'number' && 'font-technical')}>
                          {specificationValue(diff.before)}
                        </span>
                      </Table.Cell>
                      <Table.Cell class="pe-0 align-top break-words whitespace-normal">
                        <span
                          class={cn('font-medium tabular-nums', typeof diff.after === 'number' && 'font-technical')}>
                          {specificationValue(diff.after)}
                        </span>
                      </Table.Cell>
                    </Table.Row>
                  {/each}
                </Table.Body>
              </Table.Root>
              {#if item.specification.some((diff) => diff.field === 'price')}
                <p class="text-xs text-muted-foreground">{m.model_changes_price_unit()}</p>
              {/if}
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </article>
{/snippet}
<Sheet.Root bind:open>
  <Sheet.Content
    side="right"
    class="w-full! max-w-[32rem]!"
    closeLabel={m.common_close()}
    aria-describedby={undefined}
    onCloseAutoFocus={(event: Event) => {
      const target =
        restoreTarget?.isConnected && restoreTarget.getClientRects().length > 0 ? restoreTarget : focusFallback()
      if (target) {
        event.preventDefault()
        target.focus()
      }
    }}>
    <Sheet.Header><Sheet.Title>{m.model_changes_title()}</Sheet.Title></Sheet.Header>
    <div class="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-4 pb-4">
      <!-- 历史面板以浏览为主：筛选收成一行紧凑控件，选项文案自带维度名，不再逐项占用标签行。 -->
      <div class="@container flex flex-col gap-2">
        <div class="relative">
          <SearchIcon
            class="pointer-events-none absolute start-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground"
            aria-hidden="true" />
          <Input
            id="history-search"
            type="search"
            class="ps-9"
            placeholder={m.model_changes_search()}
            aria-label={m.model_changes_search()}
            bind:value={search} />
        </div>
        <div class="grid gap-2 @sm:grid-cols-3">
          <Select.Root type="single" bind:value={providerId}>
            <Select.Trigger id="history-provider" class="w-full min-w-0" aria-label={m.common_model_service()}
              ><span class="truncate"
                >{serviceOptions.get(providerId) ?? (providerId || m.model_changes_all_services())}</span
              ></Select.Trigger>
            <Select.Content
              ><Select.Group
                ><Select.Item value="">{m.model_changes_all_services()}</Select.Item
                >{#each [...serviceOptions] as [id, name] (id)}<Select.Item value={id}>{name}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content>
          </Select.Root>
          <Select.Root type="single" bind:value={type}>
            <Select.Trigger id="history-type" class="w-full min-w-0" aria-label={m.model_changes_type()}
              ><span class="truncate">{type ? changeLabel(type) : m.model_changes_all_types()}</span></Select.Trigger>
            <Select.Content
              ><Select.Group
                ><Select.Item value="">{m.model_changes_all_types()}</Select.Item
                >{#each changeTypes as kind (kind)}<Select.Item value={kind}>{changeLabel(kind)}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content>
          </Select.Root>
          <Select.Root type="single" bind:value={source}>
            <Select.Trigger id="history-source" class="w-full min-w-0" aria-label={m.model_changes_source()}
              ><span class="truncate">{source ? sourceLabel(source) : m.model_changes_all_sources()}</span
              ></Select.Trigger>
            <Select.Content
              ><Select.Group
                ><Select.Item value="">{m.model_changes_all_sources()}</Select.Item
                >{#each ['automatic_sync', 'manual_sync', 'manual_change'] as origin (origin)}<Select.Item
                    value={origin}>{sourceLabel(origin as ModelChangeSource)}</Select.Item
                  >{/each}</Select.Group
              ></Select.Content>
          </Select.Root>
        </div>
      </div>
      {#if providers.isError}<RequestFailure
          message={localizeBackendErrorMessage(providers.error)}
          retry={() => providers.refetch()} />{/if}
      {#if selectedId}
        <div bind:this={selectedElement} tabindex="-1" aria-label={m.model_changes_details()}>
          {#if selected.isPending}<p role="status">
              {m.model_changes_loading()}
            </p>{:else if selected.isError}<RequestFailure
              message={localizeBackendErrorMessage(selected.error)}
              retry={() => selected.refetch()} />{:else if selected.data}{@render batch(selected.data)}{/if}
        </div>
      {/if}
      {#if history.isError}<RequestFailure
          message={localizeBackendErrorMessage(history.error)}
          retry={() => history.refetch()} />{/if}
      {#if history.isPending}<p role="status" class="flex items-center gap-2 py-6 text-sm text-muted-foreground">
          <Spinner />{m.model_changes_loading()}
        </p>{:else if records.length === 0 && !history.isError}<Empty.Root
          ><Empty.Header
            ><Empty.Title>{filtered ? m.model_changes_no_match() : m.model_changes_empty()}</Empty.Title></Empty.Header
          ></Empty.Root
        >{:else}
        <div class="flex flex-col">
          {#each records.filter((record) => record.id !== selectedId) as record (record.id)}{@render batch(
              record,
            )}{/each}
        </div>
      {/if}
      {#if history.hasNextPage}<Button
          variant="outline"
          disabled={history.isFetchingNextPage}
          onclick={() => history.fetchNextPage()}
          >{#if history.isFetchingNextPage}<Spinner data-icon="inline-start" />{/if}{m.model_changes_older()}</Button
        >{/if}
    </div>
  </Sheet.Content>
</Sheet.Root>
