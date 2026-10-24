<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import type { MediaGenerationConfig, MediaGenerationConfigView } from '#lib/types.js'
import PageHeader from '#lib/components/page-header.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'

const validationTooltipId = 'media-generation-validation-hint'
const queryClient = useQueryClient()

const configQuery = createQuery(() => ({
  queryKey: ['media-generation-config'],
  queryFn: admin.mediaGeneration.config.get,
}))
const eligibleRoutesQuery = createQuery(() => ({
  queryKey: ['media-generation-eligible-routes'],
  queryFn: admin.mediaGeneration.eligibleRoutes,
}))

// 保存中的选择先显示；请求结束后回到服务器确认值（成功即新值，失败即旧值）。
let pendingRoute = $state<string | null | undefined>(undefined)
let routeSaving = $state(false)
let routeError = $state('')
let toggleSaving = $state(false)
let toggleError = $state('')
let validationTooltipOpen = $state(false)
let clearOpen = $state(false)
let clearSaving = $state(false)
let clearError = $state('')

const eligibleRoutes = $derived(eligibleRoutesQuery.data ?? [])
const savedRouteId = $derived(configQuery.data?.config.image.route_id ?? null)
const routeDraft = $derived(pendingRoute === undefined ? savedRouteId : pendingRoute)
const savedConfigurationValid = $derived(
  Boolean(configQuery.data?.validation.valid && configQuery.data.config.image.route_id),
)
const toggleDisabled = $derived(
  routeSaving || clearSaving || toggleSaving || (!configQuery.data?.config.enabled && !savedConfigurationValid),
)
const selectedRoute = $derived(eligibleRoutes.find((route) => route.id === routeDraft))
const selectedRouteLabel = $derived(routeDraft ? selectedRoute?.name || routeDraft : m.media_generation_no_route())
const savedRoute = $derived(eligibleRoutes.find((route) => route.id === savedRouteId))
const clearSummary = $derived(savedRouteId ? savedRoute?.name || savedRouteId : m.media_generation_no_route())

const validationMessage = $derived.by(() => {
  const validation = configQuery.data?.validation

  if (!validation || validation.valid) return ''
  if (!validation.code) return validation.message?.trim() ?? ''

  return localizeBackendErrorMessage({ code: validation.code, message: validation.message ?? undefined })
})

function updateCachedConfig(view: MediaGenerationConfigView): void {
  queryClient.setQueryData(['media-generation-config'], view)
}

async function toggleEnabled(enabled: boolean): Promise<void> {
  const current = configQuery.data

  if (
    !current ||
    routeSaving ||
    clearSaving ||
    toggleSaving ||
    enabled === current.config.enabled ||
    (enabled && !savedConfigurationValid)
  ) {
    return
  }

  toggleSaving = true
  toggleError = ''
  try {
    // 只提交服务器确认的配置，不顺带提交或丢弃未保存的路由选择。
    const input: MediaGenerationConfig = { enabled, image: { route_id: current.config.image.route_id } }
    updateCachedConfig(await admin.mediaGeneration.config.update(input))
  } catch (error) {
    toggleError = localizeBackendErrorMessage(error)
  } finally {
    toggleSaving = false
  }
}

async function persistRoute(routeId: string | null): Promise<void> {
  const current = configQuery.data

  if (!current) return

  routeSaving = true
  routeError = ''
  try {
    const input: MediaGenerationConfig = {
      enabled: current.config.enabled && Boolean(routeId),
      image: { route_id: routeId },
    }
    const view = await admin.mediaGeneration.config.update(input)
    updateCachedConfig(view)
    pendingRoute = undefined
    await queryClient.invalidateQueries({ queryKey: ['models'] })
  } catch (error) {
    // 即时保存失败：选择回到服务器确认值，错误留在本次事务内可重试。
    pendingRoute = undefined
    routeError = localizeBackendErrorMessage(error)
  } finally {
    routeSaving = false
  }
}

function chooseRoute(value?: string): void {
  const next = value ?? null

  if (next === routeDraft && !routeError) return
  pendingRoute = next
  void persistRoute(next)
}

function openClear(): void {
  clearError = ''
  clearOpen = true
}

async function unlinkRoute(): Promise<void> {
  if (clearSaving) return
  clearSaving = true
  clearError = ''
  try {
    // 解除绑定同时关闭功能，显式保存后才生效。
    const view = await admin.mediaGeneration.config.update({ enabled: false, image: { route_id: null } })
    updateCachedConfig(view)
    pendingRoute = undefined
    routeError = ''
    await queryClient.invalidateQueries({ queryKey: ['models'] })
    clearOpen = false
    toast.success(m.media_generation_settings_saved())
  } catch (error) {
    clearError = localizeBackendErrorMessage(error)
  } finally {
    clearSaving = false
  }
}
</script>

<svelte:head><title>{m.media_generation_title()} · Stravia</title></svelte:head>

<div class="route-page route-page-column">
  <PageHeader title={m.media_generation_title()} />

  {#if configQuery.isError}
    <RequestFailure
      title={m.media_generation_settings_not_loaded()}
      message={localizeBackendErrorMessage(configQuery.error)}
      retry={() => configQuery.refetch()}
      retrying={configQuery.isFetching} />
  {:else if configQuery.isPending}
    <div class="flex flex-col gap-4" aria-label={m.common_settings_loading()} aria-busy="true">
      <Skeleton class="h-12" />
      <Skeleton class="h-12" />
    </div>
  {:else if configQuery.data}
    {@const config = configQuery.data.config}
    <div>
      <SettingRow title={m.media_generation_enable()} hint={m.media_generation_enable_help()} layout="inline">
        {#snippet children({ titleId })}
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
                    aria-checked={config.enabled}
                    aria-disabled="true"
                    aria-busy={toggleSaving}
                    tabindex="0"
                    aria-labelledby={titleId}
                    aria-describedby={validationTooltipId}
                    class="inline-flex cursor-not-allowed rounded-md focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring">
                    <Switch
                      checked={config.enabled}
                      disabled
                      aria-hidden="true"
                      tabindex={-1}
                      class="pointer-events-none" />
                  </span>
                {:else}
                  <Switch
                    {...props}
                    bind:checked={() => config.enabled, (value: boolean) => void toggleEnabled(value)}
                    aria-busy={toggleSaving}
                    aria-labelledby={titleId}
                    aria-describedby={validationTooltipId} />
                {/if}
              {/snippet}
            </Tooltip.Trigger>
            <Tooltip.Content
              id={validationTooltipId}
              role="tooltip"
              sideOffset={4}
              class="max-w-[min(20rem,calc(100vw-2rem))]">
              {validationMessage || m.media_generation_route_required()}
            </Tooltip.Content>
          </Tooltip.Root>
        {/snippet}
      </SettingRow>
      {#if toggleError}
        <Alert.Root variant="destructive" class="my-3">
          <Alert.Description>{toggleError}</Alert.Description>
        </Alert.Root>
      {/if}

      <SettingRow
        title={m.media_generation_image_route_title()}
        hint={m.media_generation_image_route_hint()}
        labelFor="media-generation-image-route"
        layout="stacked"
        description={eligibleRoutesQuery.isSuccess && eligibleRoutes.length === 0 && !savedRouteId
          ? m.media_generation_no_eligible_routes()
          : undefined}>
        {#if eligibleRoutesQuery.isPending}
          <Skeleton class="h-9 min-w-0 flex-1" aria-label={m.media_generation_routes_loading()} aria-busy="true" />
        {:else}
          <Select.Root
            type="single"
            value={routeDraft ?? ''}
            onValueChange={chooseRoute}
            disabled={routeSaving || clearSaving || eligibleRoutes.length === 0}>
            <Select.Trigger id="media-generation-image-route" class="min-w-0 flex-1">
              <span class={routeDraft && !selectedRoute?.name ? 'font-technical' : ''}>
                {selectedRouteLabel}
              </span>
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
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
          {#if eligibleRoutesQuery.isSuccess && eligibleRoutes.length === 0 && !savedRouteId}
            <Button type="button" variant="outline" size="sm" href={resolve('models')}
              >{m.connect_add_a_model()}</Button>
          {/if}
          {#if routeSaving}
            <Spinner aria-label={m.media_generation_saving_route()} />
          {/if}
          {#if savedRouteId}
            <Button type="button" variant="outline" size="sm" disabled={routeSaving} onclick={openClear}
              >{m.media_generation_clear_route()}</Button>
          {/if}
        {/if}
      </SettingRow>
      {#if eligibleRoutesQuery.isError}
        <RequestFailure
          title={m.media_generation_routes_not_loaded()}
          message={localizeBackendErrorMessage(eligibleRoutesQuery.error)}
          retry={() => eligibleRoutesQuery.refetch()}
          retrying={eligibleRoutesQuery.isFetching} />
      {/if}
      {#if routeError}
        <Alert.Root variant="destructive" class="my-3">
          <Alert.Description>{routeError}</Alert.Description>
        </Alert.Root>
      {/if}
    </div>
  {/if}
</div>

<SettingsSheet
  bind:open={clearOpen}
  title={m.media_generation_clear_route_title()}
  submitLabel={m.media_generation_clear_route_submit()}
  footerNote={m.media_generation_clear_route_impact()}
  submitDisabled={savedRouteId === null}
  saving={clearSaving}
  error={clearError}
  draft={() => null}
  onsubmit={unlinkRoute}>
  <SettingRow title={m.media_generation_image_route_title()} value={clearSummary} layout="inline" />
</SettingsSheet>
