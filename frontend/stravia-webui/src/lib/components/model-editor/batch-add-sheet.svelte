<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import SearchIcon from '@lucide/svelte/icons/search'
import { SvelteMap, SvelteSet } from 'svelte/reactivity'
import { admin } from '#lib/admin-client.js'
import type { Provider, ProviderModelSummary, RoutePurpose } from '#lib/types.js'
import type { RouteTargetGroupScope } from '../route-targets-form.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Checkbox } from '#lib/components/ui/checkbox/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

interface Props {
  open: boolean
  sessionId: number
  model: string
  modelDisplayName: string
  scope: RouteTargetGroupScope
  layerNumber: number | null
  purpose: RoutePurpose | null
  providers: Provider[]
  existingProviderIds: ReadonlySet<string>
  knownInventories: ReadonlyMap<string, ProviderModelSummary[]>
  onClose: () => void
  onSubmit: (providerIds: string[], loadedInventories: ReadonlyMap<string, ProviderModelSummary[]>) => void
}

let {
  open,
  sessionId,
  model,
  modelDisplayName,
  scope,
  layerNumber,
  purpose,
  providers,
  existingProviderIds,
  knownInventories,
  onClose,
  onSubmit,
}: Props = $props()

let search = $state('')
const selectedIds = new SvelteSet<string>()
const fetchedInventories = new SvelteMap<string, ProviderModelSummary[] | null>()
let loadingInventories = $state(false)
let lastSessionId = -1

$effect(() => {
  if (!open || sessionId === lastSessionId) return
  lastSessionId = sessionId
  search = ''
  selectedIds.clear()

  const missing = providers.filter(
    (provider) => !knownInventories.has(provider.id) && !fetchedInventories.has(provider.id),
  )
  if (missing.length === 0) return

  let cancelled = false
  loadingInventories = true
  void Promise.all(
    missing.map(async (provider) => {
      try {
        const list = await admin.providers.models(provider.id)
        return [provider.id, list.models] as const
      } catch {
        return [provider.id, null] as const
      }
    }),
  ).then((entries) => {
    if (cancelled) return
    for (const [providerId, models] of entries) fetchedInventories.set(providerId, models)
    loadingInventories = false
  })

  return () => {
    cancelled = true
  }
})

interface ProviderEntry {
  provider: Provider
  alreadyInGroup: boolean
  hasSyncedModel: boolean
  inventoryChecked: boolean
  purposeMismatch: boolean
  selectable: boolean
}

const providerEntries = $derived.by<ProviderEntry[]>(() => {
  const query = search.trim().toLowerCase()
  const normalizedModel = model.trim()

  const rows: ProviderEntry[] = []
  for (const provider of providers) {
    if (
      query &&
      !provider.name.toLowerCase().includes(query) &&
      !provider.id.toLowerCase().includes(query)
    ) {
      continue
    }
    const alreadyInGroup = existingProviderIds.has(provider.id)
    const inventory = knownInventories.get(provider.id) ?? fetchedInventories.get(provider.id)
    const inventoryChecked = inventory !== undefined
    const matched = Array.isArray(inventory)
      ? inventory.find((item) => item.id === normalizedModel && item.available)
      : undefined
    const hasSyncedModel = Boolean(matched)
    const purposeMismatch = Boolean(purpose && matched && !matched.route_purposes?.includes(purpose))
    const selectable = !alreadyInGroup && !purposeMismatch
    rows.push({
      provider,
      alreadyInGroup,
      hasSyncedModel,
      inventoryChecked,
      purposeMismatch,
      selectable,
    })
  }

  return rows.toSorted((left, right) => {
    const rank = (entry: ProviderEntry) => {
      if (!entry.selectable) return 2
      return entry.hasSyncedModel ? 0 : 1
    }
    const rankDiff = rank(left) - rank(right)
    if (rankDiff !== 0) return rankDiff
    return left.provider.name.localeCompare(right.provider.name)
  })
})

const selectableEntries = $derived(providerEntries.filter((entry) => entry.selectable))
const allSelectableChecked = $derived(
  selectableEntries.length > 0 && selectableEntries.every((entry) => selectedIds.has(entry.provider.id)),
)

function scopeLabel(): string {
  return scope.enabled && layerNumber !== null
    ? m.model_editor_layer_value({ index: layerNumber })
    : m.model_editor_disabled()
}

function toggleProvider(providerId: string, checked: boolean): void {
  if (checked) selectedIds.add(providerId)
  else selectedIds.delete(providerId)
}

function toggleSelectAll(): void {
  if (allSelectableChecked) {
    for (const entry of selectableEntries) selectedIds.delete(entry.provider.id)
  } else {
    for (const entry of selectableEntries) selectedIds.add(entry.provider.id)
  }
}

function handleSubmit(): void {
  if (selectedIds.size === 0) return
  const loaded = new Map(
    [...selectedIds].flatMap((id) => {
      const inventory = knownInventories.get(id) ?? fetchedInventories.get(id)
      return Array.isArray(inventory) ? ([[id, inventory]] as const) : []
    }),
  )
  onSubmit([...selectedIds], loaded)
}
</script>

<Sheet.Root
  {open}
  onOpenChange={(nextOpen) => {
    if (!nextOpen) onClose()
  }}>
  <Sheet.Content
    side="right"
    data-slot="batch-add-sheet"
    class="w-full! gap-0 overflow-hidden p-0 sm:max-w-lg!"
    closeLabel={m.common_close()}>
    <Sheet.Header class="border-b pr-12">
      <Sheet.Title class="wrap-anywhere">{m.model_editor_batch_add_title({ model: modelDisplayName })}</Sheet.Title>
      <Sheet.Description>
        {m.model_editor_batch_add_subtitle({ scope: scopeLabel() })}
      </Sheet.Description>
    </Sheet.Header>

    <div class="route-overlay-body flex flex-col gap-3">
      <div class="flex flex-wrap items-center gap-2">
        <InputGroup.Root class="min-w-0 flex-1">
          <InputGroup.Input
            type="search"
            aria-label={m.model_editor_batch_add_search_placeholder()}
            placeholder={m.model_editor_batch_add_search_placeholder()}
            bind:value={search} />
          <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
        </InputGroup.Root>
        {#if selectableEntries.length > 0}
          <Button type="button" variant="outline" size="sm" onclick={toggleSelectAll}>
            {allSelectableChecked ? m.model_editor_batch_add_deselect_all() : m.model_editor_batch_add_select_all()}
          </Button>
        {/if}
      </div>

      {#if loadingInventories}
        <div class="flex items-center gap-2 text-xs text-muted-foreground">
          <Spinner />
          <span>{m.model_editor_batch_add_loading_inventory()}</span>
        </div>
      {/if}

      {#if providerEntries.length === 0}
        <Empty.Root class="min-h-40 border border-dashed">
          <Empty.Header>
            <Empty.Description>{m.model_editor_batch_add_empty()}</Empty.Description>
          </Empty.Header>
        </Empty.Root>
      {:else}
        <div class="divide-y rounded-lg border border-border bg-card">
          {#each providerEntries as entry (entry.provider.id)}
            {@const checked = selectedIds.has(entry.provider.id)}
            {@const checkboxId = `batch-add-provider-${entry.provider.id}`}
            <label
              for={checkboxId}
              class={[
                'flex items-center justify-between gap-3 px-3 py-2.5 text-sm transition-colors',
                entry.selectable ? 'cursor-pointer hover:bg-muted/40' : 'cursor-not-allowed opacity-60',
              ]}>
              <div class="flex min-w-0 items-center gap-2.5">
                <Checkbox
                  id={checkboxId}
                  disabled={!entry.selectable}
                  bind:checked={
                    () => entry.alreadyInGroup || checked,
                    (value: boolean) => toggleProvider(entry.provider.id, value)
                  } />
                <div class="min-w-0">
                  <span class="block truncate font-medium text-foreground">{entry.provider.name}</span>
                  <span class="block truncate font-technical text-xs text-muted-foreground">{entry.provider.id}</span>
                </div>
              </div>
              <div class="flex shrink-0 items-center gap-1.5">
                {#if entry.alreadyInGroup}
                  <Badge variant="secondary">{m.model_editor_batch_add_already_in_group()}</Badge>
                {:else if entry.purposeMismatch}
                  <Badge variant="destructive">{m.route_purpose_mismatch()}</Badge>
                {:else if entry.hasSyncedModel}
                  <Badge variant="outline" class="border-success/40 text-success">
                    {m.model_editor_batch_add_synced_model()}
                  </Badge>
                {:else if entry.inventoryChecked}
                  <span class="text-xs text-muted-foreground">{m.model_editor_batch_add_not_in_synced_list()}</span>
                {/if}
              </div>
            </label>
          {/each}
        </div>
      {/if}
    </div>

    <div class="route-overlay-footer">
      <span class="mr-auto text-xs text-muted-foreground tabular-nums">
        {m.model_editor_batch_add_selected_count({ count: selectedIds.size })}
      </span>
      <Button type="button" variant="outline" onclick={onClose}>{m.common_cancel()}</Button>
      <Button type="button" disabled={selectedIds.size === 0} onclick={handleSubmit}>
        {selectedIds.size === 1
          ? m.model_editor_batch_add_submit_single()
          : m.model_editor_batch_add_submit({ count: selectedIds.size })}
      </Button>
    </div>
  </Sheet.Content>
</Sheet.Root>
