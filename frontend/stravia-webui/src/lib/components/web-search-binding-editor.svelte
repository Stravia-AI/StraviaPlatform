<script lang="ts" module>
import type { CreateQueryResult } from '@tanstack/svelte-query'

import type { EligibleSearchModel, ExternalSearchRoute, WebSearchBackend } from '#lib/types.js'

export interface WebSearchBindingEditorProps {
  open: boolean
  /** 已确认的绑定，用作草稿基线。 */
  backend: WebSearchBackend | null | undefined
  eligibleModels: CreateQueryResult<EligibleSearchModel[]>
  externalRoutes: CreateQueryResult<ExternalSearchRoute[]>
  /** 提交完整绑定；失败 throw，Sheet 保留输入并显示原因。 */
  onsave: (backend: WebSearchBackend) => Promise<void>
}
</script>

<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import SearchCombobox from '#lib/components/search-combobox.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button, buttonVariants } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { logicalModelOption, sortLogicalModels } from '#lib/logical-model.js'

let {
  open = $bindable(false),
  backend,
  eligibleModels: eligibleModelsQuery,
  externalRoutes: externalRoutesQuery,
  onsave,
}: WebSearchBindingEditorProps = $props()

let saving = $state(false)
let backendKind = $state<'local' | 'external'>('local')
let localModelId = $state('')
let externalRouteId = $state('')
let error = $state('')

// 草稿离开保护：打开时的绑定为基线；保存中锁定提交、编辑与关闭。
const baseline = new DraftBaseline(() => ({ backendKind, localModelId, externalRouteId }))
const guard = new DraftGuard({ dirty: () => open && baseline.dirty, busy: () => saving })

let wasOpen = false
$effect.pre(() => {
  const isOpen = open
  if (isOpen && !wasOpen) untrack(loadDraft)
  wasOpen = isOpen
})

function loadDraft(): void {
  backendKind = backend?.kind ?? 'local'
  localModelId = backend?.kind === 'local' ? (backend.model_id ?? '') : ''
  externalRouteId = backend?.kind === 'external' ? (backend.route_id ?? '') : ''
  error = ''
  baseline.reset()
}

const eligibleModels = $derived(sortLogicalModels(eligibleModelsQuery.data ?? []))
// 已保存但不再可用的 Route 仍可识别，但不能被新选。
const externalRoutes = $derived.by(() => {
  const routes = externalRoutesQuery.data ?? []
  if (!externalRouteId || routes.some((route) => route.model_id === externalRouteId)) return routes
  return [
    ...routes,
    { id: externalRouteId, model_id: externalRouteId, display_name: externalRouteId, available: false },
  ]
})
const selectedExternalRoute = $derived(externalRoutes.find((route) => route.model_id === externalRouteId))
const bindingReady = $derived(
  backendKind === 'local'
    ? Boolean(localModelId)
    : Boolean(externalRouteId) && Boolean(selectedExternalRoute?.available),
)

function draftBackend(): WebSearchBackend {
  return backendKind === 'local'
    ? { kind: 'local', model_id: localModelId }
    : { kind: 'external', route_id: externalRouteId }
}

async function submit(): Promise<void> {
  if (saving || !bindingReady || !baseline.dirty) return
  saving = true
  error = ''
  try {
    await onsave(draftBackend())
    baseline.reset()
    open = false
    toast.success(m.web_search_settings_saved())
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}
</script>

<Sheet.Root bind:open={() => open, guard.openChange((next) => (open = next))}>
  <Sheet.Content
    side="right"
    class="route-overlay-content w-full! gap-0 overflow-hidden p-0 sm:max-w-[32rem]!"
    closeLabel={m.common_close()}
    locked={guard.busy}>
    <Sheet.Header class="border-b pr-12">
      <Sheet.Title>{m.web_search_edit_method()}</Sheet.Title>
    </Sheet.Header>
    <form
      class="route-overlay-form"
      onsubmit={(event) => {
        event.preventDefault()
        void submit()
      }}>
      <div class="route-overlay-body flex flex-col gap-4">
        <Field.Group>
          <Field.Field>
            <Field.Label for="search-backend">{m.web_search_method()}</Field.Label>
            <Select.Root type="single" bind:value={backendKind} disabled={saving}>
              <Select.Trigger id="search-backend" class="w-full">
                {backendKind === 'local' ? m.web_search_use_stravia_model() : m.web_search_use_external_route()}
              </Select.Trigger>
              <Select.Content
                ><Select.Group>
                  <Select.Item value="local" label={m.web_search_use_stravia_model()}>
                    {m.web_search_use_stravia_model()}
                  </Select.Item>
                  <Select.Item value="external" label={m.web_search_use_external_route()}>
                    {m.web_search_use_external_route()}
                  </Select.Item>
                </Select.Group></Select.Content>
            </Select.Root>
          </Field.Field>

          {#if backendKind === 'local'}
            <Field.Field>
              <Field.Label for="search-local-model" hint={m.web_search_eligible_model_help()}
                >{m.web_search_model_used()}</Field.Label>
              <SearchCombobox
                id="search-local-model"
                value={localModelId}
                options={eligibleModels.map(logicalModelOption)}
                placeholder={m.common_select_model()}
                emptyText={m.common_no_matching_models()}
                disabled={saving || eligibleModelsQuery.isPending || eligibleModelsQuery.isError}
                onValueChange={(value: string) => (localModelId = value)} />
            </Field.Field>
            {#if eligibleModelsQuery.isError}
              <RequestFailure
                message={localizeBackendErrorMessage(eligibleModelsQuery.error)}
                retry={() => eligibleModelsQuery.refetch()}
                retrying={eligibleModelsQuery.isFetching} />
            {:else if eligibleModelsQuery.isPending}
              <p class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
            {:else if eligibleModels.length === 0}
              <div class="flex flex-wrap items-center gap-x-2" role="status">
                <span class="text-sm text-muted-foreground">{m.web_search_no_eligible_models()}</span>
                <a href={resolve('/models')} class={buttonVariants({ variant: 'link', size: 'sm' })}
                  >{m.web_search_manage_models()}</a>
              </div>
            {/if}
          {:else}
            <Field.Field data-invalid={Boolean(externalRouteId) && !selectedExternalRoute?.available}>
              <Field.Label for="search-external-route">{m.web_search_external_route()}</Field.Label>
              <SearchCombobox
                id="search-external-route"
                value={externalRouteId}
                options={externalRoutes.map((route) => ({
                  ...logicalModelOption({
                    id: route.model_id,
                    model_id: route.model_id,
                    display_name: route.display_name,
                  }),
                  disabled: !route.available,
                }))}
                placeholder={m.web_search_select_external_route()}
                emptyText={m.common_no_matching_models()}
                disabled={saving || externalRoutesQuery.isPending || externalRoutesQuery.isError}
                onValueChange={(value: string) => (externalRouteId = value)} />
              {#if externalRouteId && !selectedExternalRoute?.available}
                <Field.Error>{m.web_search_external_route_unavailable()}</Field.Error>
              {/if}
            </Field.Field>
            {#if externalRoutesQuery.isError}
              <RequestFailure
                title={m.web_search_external_routes_not_loaded()}
                message={localizeBackendErrorMessage(externalRoutesQuery.error)}
                retry={() => externalRoutesQuery.refetch()}
                retrying={externalRoutesQuery.isFetching} />
            {:else if externalRoutesQuery.isPending}
              <p class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</p>
            {/if}
          {/if}
        </Field.Group>
        {#if error}
          <Alert.Root variant="destructive" role="alert">
            <Alert.Description>{error}</Alert.Description>
          </Alert.Root>
        {/if}
      </div>
      <Sheet.Footer class="route-overlay-footer pb-[max(0.75rem,env(safe-area-inset-bottom))]">
        <Button type="button" variant="outline" disabled={saving} onclick={() => guard.request(() => (open = false))}
          >{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving || !bindingReady || !baseline.dirty} aria-busy={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.web_search_save_method()}
        </Button>
      </Sheet.Footer>
    </form>
  </Sheet.Content>
</Sheet.Root>

<DraftDiscardDialog {guard} />
