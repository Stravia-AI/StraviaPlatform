<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'

import { admin, platformLlmQueryKey, platformLlmQueryOptions } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import {
  effectiveModelDisplayName,
  logicalModelOption,
  logicalModelSecondaryId,
  sortLogicalModels,
} from '#lib/logical-model.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import SearchCombobox from '#lib/components/search-combobox.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { buttonVariants } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'

const queryClient = useQueryClient()
const configQuery = createQuery(() => platformLlmQueryOptions())
const config = $derived(configQuery.data ?? null)

let editorOpen = $state(false)
let draftRouteId = $state('')
let saving = $state(false)
let saveError = $state('')

const savedRouteId = $derived(config?.route_id ?? '')
const eligibleRoutes = $derived(sortLogicalModels(config?.eligible_routes ?? []))
const routeOptions = $derived(eligibleRoutes.map(logicalModelOption))
const selectedRoute = $derived(eligibleRoutes.find((route) => route.id === draftRouteId))
const hasChanges = $derived(draftRouteId !== savedRouteId)
const clearingBinding = $derived(Boolean(savedRouteId) && !draftRouteId)
const draftUnavailable = $derived(Boolean(draftRouteId) && !selectedRoute)
const draftValid = $derived(!draftRouteId || Boolean(selectedRoute))
const canSave = $derived(hasChanges && draftValid)

const availableRoute = $derived(config?.status === 'available' ? config.route : null)
const availableDisplayName = $derived(availableRoute ? effectiveModelDisplayName(availableRoute) : '')
const availableSecondaryId = $derived(availableRoute ? logicalModelSecondaryId(availableRoute) : undefined)

function openEditor(): void {
  if (!config) return
  draftRouteId = savedRouteId
  saveError = ''
  editorOpen = true
}

function selectRoute(value: string): void {
  if (value === draftRouteId) return
  draftRouteId = value
  saveError = ''
}

function resetEditor(): void {
  draftRouteId = savedRouteId
  saveError = ''
}

async function save(): Promise<void> {
  if (saving || !canSave) return
  saving = true
  saveError = ''
  try {
    const view = await admin.platformLlm.update({ route_id: draftRouteId || null })
    queryClient.setQueryData(platformLlmQueryKey, view)
    editorOpen = false
    toast.success(view.route_id ? m.platform_llm_saved() : m.platform_llm_cleared())
  } catch (cause) {
    saveError = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<SettingsSection id="platform-llm" title={m.platform_llm_section_title()}>
  {#if configQuery.isError}
    <div class="p-4">
      <RequestFailure
        title={m.platform_llm_not_loaded()}
        message={localizeBackendErrorMessage(configQuery.error)}
        retry={() => void configQuery.refetch()}
        retrying={configQuery.isFetching} />
    </div>
  {:else if configQuery.isPending}
    <div class="flex flex-col gap-3 p-4" aria-busy="true" aria-label={m.common_settings_loading()}>
      <Skeleton class="h-9" />
    </div>
  {:else if config}
    <SettingRow title={m.platform_llm_model_title()} hint={m.platform_llm_model_description()} onedit={openEditor}>
      {#if config.status === 'available'}
        <div
          class="flex min-w-0 flex-1 flex-col gap-0.5 @min-[32rem]/setting-row:max-w-[20rem] @min-[32rem]/setting-row:flex-initial @min-[32rem]/setting-row:items-end">
          {#if availableRoute && availableSecondaryId}
            <span class="max-w-full truncate text-sm font-medium text-foreground">{availableDisplayName}</span>
            <div class="flex max-w-full min-w-0">
              <TechnicalValue value={availableSecondaryId} class="text-muted-foreground" />
            </div>
          {:else if availableRoute}
            <div class="flex max-w-full min-w-0">
              <TechnicalValue value={availableRoute.model_id} class="text-foreground" />
            </div>
          {:else if config.route_id}
            <div class="flex max-w-full min-w-0">
              <TechnicalValue value={config.route_id} class="text-foreground" />
            </div>
          {/if}
        </div>
      {:else if config.status === 'unavailable'}
        <div
          class="flex min-w-0 flex-1 flex-col gap-0.5 @min-[32rem]/setting-row:max-w-[20rem] @min-[32rem]/setting-row:flex-initial @min-[32rem]/setting-row:items-end">
          <StatusIndicator compact tone="warning" label={m.common_unavailable()} />
          {#if config.route_id}
            <div class="flex max-w-full min-w-0">
              <TechnicalValue value={config.route_id} class="text-muted-foreground" />
            </div>
          {/if}
        </div>
      {:else}
        <span class="min-w-0 flex-1 text-sm text-muted-foreground @min-[32rem]/setting-row:flex-initial">
          {m.common_not_configured()}
        </span>
      {/if}
    </SettingRow>
    {#if config.status === 'unavailable'}
      <div class="px-4 pb-3">
        <Alert.Root variant="warning" role="status">
          <Alert.Description>{m.platform_llm_saved_unavailable()}</Alert.Description>
        </Alert.Root>
      </div>
    {/if}
  {/if}
</SettingsSection>

<SettingsSheet
  bind:open={editorOpen}
  title={savedRouteId ? m.platform_llm_edit_title() : m.platform_llm_configure_title()}
  draft={() => draftRouteId}
  submitLabel={m.common_save_settings()}
  submitDisabled={!canSave}
  {saving}
  error={saveError}
  onsubmit={save}
  onclose={resetEditor}>
  <Field.Group>
    <Field.Field data-invalid={draftUnavailable ? true : undefined}>
      <Field.Label for="platform-llm-route" hint={m.platform_llm_eligible_models_help()}>
        {m.common_model()}
      </Field.Label>
      <SearchCombobox
        id="platform-llm-route"
        value={draftRouteId}
        options={routeOptions}
        clearLabel={m.platform_llm_clear_selection()}
        placeholder={m.common_select_model()}
        emptyText={eligibleRoutes.length === 0
          ? m.platform_llm_no_eligible_models_empty()
          : m.common_no_matching_models()}
        disabled={saving}
        onValueChange={selectRoute} />
      {#if clearingBinding}
        <Field.Description>{m.platform_llm_clear_impact()}</Field.Description>
      {:else if draftUnavailable}
        <Field.Error>{m.platform_llm_draft_unavailable()}</Field.Error>
      {/if}
    </Field.Field>
    {#if eligibleRoutes.length === 0}
      <div class="flex flex-wrap items-center gap-x-2" role="status">
        <span class="text-sm text-muted-foreground">{m.platform_llm_no_eligible_models()}</span>
        <a href={resolve('/models')} class={buttonVariants({ variant: 'link', size: 'sm' })}>
          {m.platform_llm_manage_models()}
        </a>
      </div>
    {/if}
  </Field.Group>
</SettingsSheet>
