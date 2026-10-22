<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import type { MediaGenerationConfig, MediaGenerationConfigView } from '#lib/types.js'
import PageHeader from '#lib/components/page-header.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'

const UNBOUND_ROUTE = '__media_generation_unbound__'
const queryClient = useQueryClient()

const configQuery = createQuery(() => ({
  queryKey: ['media-generation-config'],
  queryFn: admin.mediaGeneration.config.get,
}))
const eligibleRoutesQuery = createQuery(() => ({
  queryKey: ['media-generation-eligible-routes'],
  queryFn: admin.mediaGeneration.eligibleRoutes,
}))

let initialized = $state(false)
let routeId = $state<string | null>(null)
let toggleSaving = $state(false)
let toggleError = $state('')
let validationTooltipOpen = $state(false)
let saving = $state(false)
let saveError = $state('')
const eligibleRoutes = $derived(eligibleRoutesQuery.data ?? [])
const savedRouteId = $derived(configQuery.data?.config.image.route_id ?? null)
const hasChanges = $derived(Boolean(configQuery.data) && routeId !== savedRouteId)
const savedConfigurationValid = $derived(
  Boolean(configQuery.data?.validation.valid && configQuery.data.config.image.route_id),
)
const toggleDisabled = $derived(
  saving || toggleSaving || (!configQuery.data?.config.enabled && !savedConfigurationValid),
)
const canSave = $derived(Boolean(configQuery.data) && hasChanges && !saving && !toggleSaving)
const selectedRoute = $derived(eligibleRoutes.find((route) => route.id === routeId))

const selectedRouteLabel = $derived(routeId ? selectedRoute?.name || routeId : m.media_generation_no_route())

const validationMessage = $derived.by(() => {
  const validation = configQuery.data?.validation

  if (!validation || validation.valid) return ''
  if (!validation.code) return validation.message?.trim() ?? ''

  return localizeBackendErrorMessage({ code: validation.code, message: validation.message ?? undefined })
})

$effect(() => {
  const view = configQuery.data

  if (!view || initialized) return

  routeId = view.config.image.route_id
  initialized = true
})

function updateCachedConfig(view: MediaGenerationConfigView): void {
  queryClient.setQueryData(['media-generation-config'], view)
}

async function toggleEnabled(enabled: boolean): Promise<void> {
  const current = configQuery.data

  if (
    !current ||
    saving ||
    toggleSaving ||
    enabled === current.config.enabled ||
    (enabled && !savedConfigurationValid)
  ) {
    return
  }

  toggleSaving = true
  toggleError = ''
  try {
    const input: MediaGenerationConfig = { enabled, image: { route_id: current.config.image.route_id } }
    updateCachedConfig(await admin.mediaGeneration.config.update(input))
  } catch (error) {
    toggleError = localizeBackendErrorMessage(error)
  } finally {
    toggleSaving = false
  }
}

async function saveRoute(): Promise<void> {
  const current = configQuery.data

  if (!current || !canSave) return

  saving = true
  saveError = ''

  try {
    const input: MediaGenerationConfig = {
      enabled: current.config.enabled && Boolean(routeId),
      image: { route_id: routeId },
    }
    const view = await admin.mediaGeneration.config.update(input)
    routeId = view.config.image.route_id
    updateCachedConfig(view)
    await queryClient.invalidateQueries({ queryKey: ['models'] })
    toast.success(m.media_generation_settings_saved())
  } catch (error) {
    saveError = localizeBackendErrorMessage(error)
  } finally {
    saving = false
  }
}
</script>

<svelte:head><title>{m.media_generation_title()} · Stravia</title></svelte:head>

<div class="route-page">
  <PageHeader title={m.media_generation_title()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.media_generation_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {:else if configQuery.isPending}
    <div class="flex flex-col gap-4" aria-label={m.common_settings_loading()} aria-busy="true">
      <Skeleton class="h-24" />
      <Skeleton class="h-40" />
    </div>
  {:else if configQuery.data}
    <section class="route-section" aria-labelledby="media-generation-gate-title">
      <div class="route-section-header">
        <div class="min-w-0 flex-1 basis-64">
          <h2 id="media-generation-gate-title" class="route-section-title">{m.media_generation_enable()}</h2>
          <p id="media-generation-gate-description" class="route-section-description">
            {m.media_generation_enable_help()}
          </p>
        </div>
        <div class="flex shrink-0 items-center gap-3">
          {#if toggleSaving}<Spinner aria-label={m.media_generation_saving_enabled_state()} />{/if}
          <Tooltip.Root
            open={validationTooltipOpen && !savedConfigurationValid}
            onOpenChange={(open: boolean) => (validationTooltipOpen = open)}
            disabled={savedConfigurationValid}
            delayDuration={0}
            disableCloseOnTriggerClick>
            <Tooltip.Trigger onpointerdown={() => (validationTooltipOpen = true)}>
              {#snippet child({ props })}
                {#if toggleDisabled}
                  <span
                    {...props}
                    role="switch"
                    aria-checked={configQuery.data?.config.enabled ?? false}
                    aria-disabled="true"
                    aria-busy={toggleSaving}
                    tabindex="0"
                    aria-labelledby="media-generation-gate-title"
                    aria-describedby="media-generation-gate-description media-generation-validation-hint"
                    class="inline-flex cursor-not-allowed rounded-md focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring">
                    <Switch
                      checked={configQuery.data?.config.enabled ?? false}
                      disabled
                      aria-hidden="true"
                      tabindex={-1}
                      class="pointer-events-none" />
                  </span>
                {:else}
                  <Switch
                    {...props}
                    bind:checked={
                      () => configQuery.data?.config.enabled ?? false, (value: boolean) => void toggleEnabled(value)
                    }
                    aria-busy={toggleSaving}
                    aria-labelledby="media-generation-gate-title"
                    aria-describedby={savedConfigurationValid
                      ? 'media-generation-gate-description'
                      : 'media-generation-gate-description media-generation-validation-hint'} />
                {/if}
              {/snippet}
            </Tooltip.Trigger>
            <Tooltip.Content
              id="media-generation-validation-hint"
              role="tooltip"
              sideOffset={4}
              class="max-w-[min(20rem,calc(100vw-2rem))]">
              {validationMessage || m.media_generation_route_required()}
            </Tooltip.Content>
          </Tooltip.Root>
        </div>
      </div>

      {#if toggleError}
        <Alert.Root variant="destructive"><Alert.Description>{toggleError}</Alert.Description></Alert.Root>
      {/if}
    </section>

    <section class="route-section" aria-labelledby="media-generation-image-route-title">
      <div class="route-section-header">
        <div>
          <h2 id="media-generation-image-route-title" class="route-section-title">
            {m.media_generation_image_route_title()}
          </h2>
          <p class="route-section-description">{m.media_generation_image_route_help()}</p>
        </div>
        <Button disabled={!canSave} aria-busy={saving} onclick={() => void saveRoute()}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save_settings()}
        </Button>
      </div>

      <div class="flex flex-col gap-3">
        {#if hasChanges}
          <p class="text-sm text-muted-foreground" role="status">
            {routeId ? m.media_generation_route_unsaved() : m.media_generation_route_clear_pending()}
          </p>
        {/if}
        {#if saveError}
          <Alert.Root variant="destructive"><Alert.Description>{saveError}</Alert.Description></Alert.Root>
        {/if}
      </div>

      {#if eligibleRoutesQuery.isPending}
        <div class="flex flex-col gap-3" aria-label={m.media_generation_routes_loading()} aria-busy="true">
          <Skeleton class="h-5 w-36" />
          <Skeleton class="h-10 w-full max-w-[28rem]" />
        </div>
      {:else if eligibleRoutesQuery.isError}
        <RequestFailure
          title={m.media_generation_routes_not_loaded()}
          message={localizeBackendErrorMessage(eligibleRoutesQuery.error)}
          retry={() => eligibleRoutesQuery.refetch()}
          retrying={eligibleRoutesQuery.isFetching} />
      {:else}
        <Field.Group>
          <Field.Field size="select">
            <Field.Label for="media-generation-image-route">{m.media_generation_route_label()}</Field.Label>
            <Select.Root
              type="single"
              value={routeId ?? UNBOUND_ROUTE}
              onValueChange={(value: string) => {
                routeId = value === UNBOUND_ROUTE ? null : value
                saveError = ''
              }}
              disabled={saving || toggleSaving}>
              <Select.Trigger id="media-generation-image-route" class="w-full">
                <span class={routeId && !selectedRoute?.name ? 'font-technical' : ''}>
                  {selectedRouteLabel}
                </span>
              </Select.Trigger>
              <Select.Content>
                <Select.Group>
                  <Select.Item value={UNBOUND_ROUTE} label={m.media_generation_no_route()}>
                    {m.media_generation_no_route()}
                  </Select.Item>
                  {#each eligibleRoutes as route (route.id)}
                    <Select.Item value={route.id} label={route.name || route.id}>
                      <span class="flex min-w-0 flex-col">
                        <span class="truncate">{route.name || route.id}</span>
                        {#if route.name}
                          <span class="truncate font-technical text-xs text-muted-foreground">{route.id}</span>
                        {/if}
                      </span>
                    </Select.Item>
                  {/each}
                </Select.Group>
              </Select.Content>
            </Select.Root>
          </Field.Field>
        </Field.Group>
      {/if}
    </section>
  {/if}

  <section class="route-section" aria-labelledby="media-generation-support-title">
    <div class="route-section-header">
      <div>
        <h2 id="media-generation-support-title" class="route-section-title">
          {m.media_generation_supported_types_title()}
        </h2>
        <p class="route-section-description">{m.media_generation_supported_types_help()}</p>
      </div>
      <Badge variant="secondary">{m.media_generation_image_type()}</Badge>
    </div>
  </section>
</div>
