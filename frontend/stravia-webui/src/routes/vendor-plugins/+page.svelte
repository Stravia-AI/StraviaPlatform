<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import CircleAlertIcon from '@lucide/svelte/icons/circle-alert'
import PackageOpenIcon from '@lucide/svelte/icons/package-open'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import UploadIcon from '@lucide/svelte/icons/upload'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import type {
  PluginBindingImpact,
  PluginNetworkPermission,
  PluginPreview,
  PluginSource,
  PluginSummary,
} from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import RequestFailure from '$lib/components/request-failure.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import * as Alert from '$lib/components/ui/alert'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Card from '$lib/components/ui/card'
import { Checkbox } from '$lib/components/ui/checkbox'
import * as Dialog from '$lib/components/ui/dialog'
import * as Empty from '$lib/components/ui/empty'
import { Label } from '$lib/components/ui/label'
import * as Sheet from '$lib/components/ui/sheet'
import { Skeleton } from '$lib/components/ui/skeleton'
import { Spinner } from '$lib/components/ui/spinner'
import { cn } from '$lib/utils'

const MAX_PLUGIN_BYTES = 64 * 1024 * 1024
const queryClient = useQueryClient()
const pluginsQuery = createQuery(() => ({ queryKey: ['vendor-plugins'], queryFn: admin.vendorPlugins.list }))

let fileInput = $state<HTMLInputElement | null>(null)
let selectedFiles = $state<FileList>()
let importError = $state<string>()
let importing = $state(false)
let restoringVendorId = $state<string>()
let restoreError = $state<{ vendorId: string; message: string }>()
let preview = $state<PluginPreview>()
let previewOpen = $state(false)
let confirmError = $state<string>()
let confirming = $state(false)
let allowDataDiscard = $state(false)
let uninstallTarget = $state<PluginSummary>()
let uninstallOpen = $state(false)
let uninstalling = $state(false)
let uninstallError = $state<string>()
let detailVendorId = $state<string>()
let detailOpen = $state(false)

const plugins = $derived(pluginsQuery.data ?? [])
const selectedFile = $derived(selectedFiles?.item(0) ?? undefined)
const detailPlugin = $derived(plugins.find((plugin) => plugin.vendor_id === detailVendorId))
const pendingCount = $derived(plugins.filter((plugin) => plugin.pending_update).length)
const failedCount = $derived(plugins.filter((plugin) => plugin.error || /error|fail/i.test(plugin.status)).length)
const attentionNotes = $derived(
  [
    pendingCount > 0 ? m.vendor_plugins_attention_pending({ count: pendingCount }) : undefined,
    failedCount > 0 ? m.vendor_plugins_attention_errors({ count: failedCount }) : undefined,
  ].filter((note) => note !== undefined),
)
const dataDiscardRequired = $derived((preview?.discarded_data.length ?? 0) > 0)
const addedPermissions = $derived(preview?.network_permissions.filter((item) => item.added) ?? [])
const retainedPermissions = $derived(preview?.network_permissions.filter((item) => !item.added) ?? [])
const removedPermissions = $derived(preview?.removed_network_permissions ?? [])
const networkChanges = $derived(addedPermissions.length + removedPermissions.length)
const discardKindCount = $derived(
  preview?.discarded_data.reduce((total, discard) => total + discard.kinds.length, 0) ?? 0,
)
const sessionsCancelled = $derived(
  Boolean(preview?.cancels_active_operations && (preview?.affected_auth_sessions ?? 0) > 0),
)
const hasRisks = $derived(
  Boolean(preview) &&
    (dataDiscardRequired ||
      networkChanges > 0 ||
      (preview?.affected_bindings.length ?? 0) > 0 ||
      Boolean(preview?.cancels_active_operations) ||
      sessionsCancelled ||
      ((preview?.affected_providers.length ?? 0) > 0 && !preview?.inherits_credentials)),
)
const networkTileValue = $derived.by(() => {
  if (!preview || networkChanges === 0) return m.vendor_plugins_preview_impact_none()
  const parts: string[] = []
  if (addedPermissions.length > 0) parts.push(`+${addedPermissions.length}`)
  if (removedPermissions.length > 0) parts.push(`−${removedPermissions.length}`)
  return parts.join(' / ')
})
const safeNotes = $derived.by(() => {
  if (!preview) return []
  const notes: string[] = []
  if (preview.affected_providers.length === 0) {
    notes.push(m.vendor_plugins_preview_no_connections())
  } else if (preview.inherits_credentials) {
    notes.push(m.vendor_plugins_preview_safe_credentials({ count: preview.affected_providers.length }))
  }
  if (!preview.cancels_active_operations) {
    notes.push(m.vendor_plugins_preview_safe_operations())
  }
  if (!sessionsCancelled) {
    notes.push(m.vendor_plugins_preview_auth_sessions_none())
  }
  if (retainedPermissions.length > 0) {
    notes.push(m.vendor_plugins_preview_safe_network({ count: retainedPermissions.length }))
  }
  if (preview.affected_bindings.length === 0) {
    notes.push(m.vendor_plugins_preview_binding_none())
  }
  if (preview.discarded_data.length === 0) {
    notes.push(m.vendor_plugins_preview_data_compatible())
  }
  return notes
})

function sourceLabel(source: PluginSource): string {
  return source === 'builtin' ? m.vendor_plugins_source_builtin() : m.vendor_plugins_source_local()
}

function statusLabel(status: string): string {
  switch (status.toLowerCase()) {
    case 'active':
    case 'loaded':
    case 'ready':
      return m.vendor_plugins_status_ready()
    case 'error':
    case 'failed':
      return m.vendor_plugins_status_error()
    case 'pending':
    case 'pending_confirmation':
    case 'pending_update':
      return m.vendor_plugins_status_pending()
    case 'unavailable':
      return m.vendor_plugins_status_unavailable()
    default:
      return status || '—'
  }
}

function statusTone(status: string, error: string | null): 'healthy' | 'warning' | 'error' | 'neutral' {
  if (error || /error|fail/i.test(status)) return 'error'
  if (/pending|unavailable|degraded/i.test(status)) return 'warning'
  if (/active|loaded|ready/i.test(status)) return 'healthy'
  return 'neutral'
}

function bindingLabel(binding: PluginBindingImpact): string {
  return binding.upstream_model
    ? m.vendor_plugins_binding_model_value({
        route: binding.route_id,
        provider: binding.provider_id,
        model: binding.upstream_model,
      })
    : m.vendor_plugins_binding_value({ route: binding.route_id, provider: binding.provider_id })
}

function canRestoreBuiltin(plugin: PluginSummary): boolean {
  return Boolean(
    plugin.builtin_version &&
    !plugin.pending_update &&
    (plugin.source === 'local' || plugin.status !== 'ready' || plugin.version !== plugin.builtin_version),
  )
}

function canUninstall(plugin: PluginSummary): boolean {
  return plugin.vendor_id !== 'base'
}

function pickPluginFile(): void {
  if (importing) return
  importError = undefined
  fileInput?.click()
}

function openDetails(plugin: PluginSummary): void {
  detailVendorId = plugin.vendor_id
  detailOpen = true
}

function showPreview(value: PluginPreview): void {
  preview = value
  allowDataDiscard = false
  confirmError = undefined
  previewOpen = true
}

function setPreviewOpen(open: boolean): void {
  if (!open && confirming) return
  previewOpen = open
  if (!open) {
    preview = undefined
    allowDataDiscard = false
    confirmError = undefined
  }
}

async function importPlugin(): Promise<void> {
  const file = selectedFile
  selectedFiles = undefined
  if (fileInput) fileInput.value = ''
  importError = undefined
  if (!file) return
  if (file.size > MAX_PLUGIN_BYTES) {
    importError = m.vendor_plugins_file_too_large()
    return
  }

  importing = true
  try {
    showPreview(await admin.vendorPlugins.import(file))
  } catch (error) {
    importError = localizeBackendErrorMessage(error)
  } finally {
    importing = false
  }
}

async function restoreBuiltin(vendorId: string): Promise<void> {
  restoringVendorId = vendorId
  restoreError = undefined
  try {
    showPreview(await admin.vendorPlugins.restoreBuiltin(vendorId))
  } catch (error) {
    restoreError = { vendorId, message: localizeBackendErrorMessage(error) }
  } finally {
    restoringVendorId = undefined
  }
}

function openUninstall(plugin: PluginSummary): void {
  if (plugin.vendor_id === 'base') return
  uninstallTarget = plugin
  uninstallError = undefined
  uninstallOpen = true
}

function setUninstallOpen(open: boolean): void {
  if (uninstalling) return
  uninstallOpen = open
  if (!open) {
    uninstallTarget = undefined
    uninstallError = undefined
  }
}

async function refreshPluginRuntimeQueries(): Promise<void> {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ['vendor-plugins'] }),
    queryClient.invalidateQueries({ queryKey: ['gateway-status'] }),
    queryClient.invalidateQueries({ queryKey: ['provider-descriptors'] }),
    queryClient.invalidateQueries({ queryKey: ['providers'] }),
    queryClient.invalidateQueries({ queryKey: ['provider-models'] }),
    queryClient.invalidateQueries({ queryKey: ['provider-allowances'] }),
    queryClient.invalidateQueries({ queryKey: ['provider-allowance'] }),
    queryClient.invalidateQueries({ queryKey: ['model-target-statuses'] }),
    queryClient.invalidateQueries({ queryKey: ['models'] }),
    queryClient.invalidateQueries({ queryKey: ['web-providers'] }),
    queryClient.invalidateQueries({ queryKey: ['web-access-settings'] }),
    queryClient.invalidateQueries({ queryKey: ['web-search-config'] }),
    queryClient.invalidateQueries({ queryKey: ['web-search-eligible-models'] }),
    queryClient.invalidateQueries({ queryKey: ['web-search-external-routes'] }),
    queryClient.invalidateQueries({ queryKey: ['media-understanding-config'] }),
    queryClient.invalidateQueries({ queryKey: ['media-generation-config'] }),
    queryClient.invalidateQueries({ queryKey: ['media-generation-eligible-routes'] }),
    queryClient.invalidateQueries({ queryKey: ['oauth-session'] }),
  ])
}

async function uninstallPlugin(): Promise<void> {
  const target = uninstallTarget
  if (!target || uninstalling || target.vendor_id === 'base') return

  uninstalling = true
  uninstallError = undefined
  try {
    await admin.vendorPlugins.uninstall(target.vendor_id)
    await refreshPluginRuntimeQueries()
    uninstallOpen = false
    uninstallTarget = undefined
    detailOpen = false
    toast.success(m.vendor_plugins_uninstalled())
  } catch (error) {
    uninstallError = localizeBackendErrorMessage(error)
  } finally {
    uninstalling = false
  }
}

async function confirmPreview(): Promise<void> {
  const current = preview
  if (!current || confirming || (dataDiscardRequired && !allowDataDiscard)) return

  confirming = true
  confirmError = undefined
  try {
    await admin.vendorPlugins.confirm({
      preview_id: current.id,
      allow_data_discard: dataDiscardRequired ? allowDataDiscard : false,
    })
    await queryClient.invalidateQueries({ queryKey: ['vendor-plugins'] })
    previewOpen = false
    preview = undefined
    toast.success(m.vendor_plugins_update_applied())
  } catch (error) {
    confirmError = localizeBackendErrorMessage(error)
  } finally {
    confirming = false
  }
}

function networkPermissionContext(permission: PluginNetworkPermission): string[] {
  const context: string[] = []
  if (permission.provider_id) {
    context.push(m.vendor_plugins_preview_network_provider({ provider: permission.provider_id }))
  }
  if (permission.configuration_field) {
    context.push(m.vendor_plugins_preview_network_field({ field: permission.configuration_field }))
  }
  return context
}
</script>

<svelte:head><title>{m.vendor_plugins_title()} · Stravia</title></svelte:head>

<div class="route-page">
  <PageHeader
    eyebrow={m.vendor_plugins_eyebrow()}
    title={m.vendor_plugins_title()}
    description={m.vendor_plugins_summary()}>
    {#snippet actions()}
      <Button variant="outline" disabled={pluginsQuery.isFetching} onclick={() => pluginsQuery.refetch()}>
        {#if pluginsQuery.isFetching}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
            data-icon="inline-start" />{/if}
        {m.vendor_plugins_refresh()}
      </Button>
      <Button onclick={pickPluginFile} disabled={importing} aria-busy={importing}>
        {#if importing}<Spinner data-icon="inline-start" />{:else}<UploadIcon data-icon="inline-start" />{/if}
        {importing ? m.vendor_plugins_importing() : m.vendor_plugins_import_title()}
      </Button>
    {/snippet}
  </PageHeader>

  <input
    bind:this={fileInput}
    bind:files={selectedFiles}
    type="file"
    accept=".wasm,application/wasm"
    class="hidden"
    onchange={() => void importPlugin()} />

  {#if pluginsQuery.error}
    <RequestFailure
      title={m.vendor_plugins_load_failed()}
      message={localizeBackendErrorMessage(pluginsQuery.error)}
      retry={() => pluginsQuery.refetch()}
      retrying={pluginsQuery.isFetching} />
  {/if}

  {#if attentionNotes.length > 0}
    <Alert.Root variant="warning">
      <CircleAlertIcon />
      <Alert.Title>{m.vendor_plugins_attention_title()}</Alert.Title>
      <Alert.Description>{attentionNotes.join(' · ')}</Alert.Description>
    </Alert.Root>
  {/if}

  {#if importError}
    <Alert.Root variant="destructive">
      <CircleAlertIcon />
      <Alert.Description>{importError}</Alert.Description>
    </Alert.Root>
  {/if}

  {#if pluginsQuery.isPending}
    <div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3" aria-busy="true">
      {#each Array(4) as _, index (index)}
        <Card.Root>
          <Card.Header><Skeleton class="h-5 w-40" /><Skeleton class="h-4 w-56" /></Card.Header>
          <Card.Content class="flex flex-col gap-3"><Skeleton class="h-12" /><Skeleton class="h-8" /></Card.Content>
        </Card.Root>
      {/each}
    </div>
  {:else if plugins.length === 0 && !pluginsQuery.error}
    <Empty.Root class="border border-dashed">
      <Empty.Header>
        <Empty.Media variant="icon"><PackageOpenIcon /></Empty.Media>
        <Empty.Title>{m.vendor_plugins_empty_title()}</Empty.Title>
        <Empty.Description>{m.vendor_plugins_empty_description()}</Empty.Description>
      </Empty.Header>
      <Empty.Content>
        <Button onclick={pickPluginFile} disabled={importing}>
          <UploadIcon data-icon="inline-start" />
          {m.vendor_plugins_import_title()}
        </Button>
      </Empty.Content>
    </Empty.Root>
  {:else if plugins.length > 0}
    <div class="grid items-start gap-4 sm:grid-cols-2 xl:grid-cols-3">
      {#each plugins as plugin (plugin.vendor_id)}
        <Card.Root>
          <Card.Header>
            <Card.Title class="flex min-w-0 items-center gap-2.5">
              <span
                class="font-structural flex size-8 shrink-0 items-center justify-center rounded-lg bg-accent text-xs font-semibold text-accent-foreground"
                aria-hidden="true">{plugin.name.charAt(0).toUpperCase()}</span>
              <span class="truncate">{plugin.name}</span>
            </Card.Title>
            <Card.Description class="font-technical truncate">{plugin.vendor_id}</Card.Description>
            <Card.Action>
              <StatusIndicator label={statusLabel(plugin.status)} tone={statusTone(plugin.status, plugin.error)} />
            </Card.Action>
          </Card.Header>
          <Card.Content class="flex flex-col gap-3">
            <dl class="grid grid-cols-2 gap-x-4 gap-y-3 text-sm sm:grid-cols-3">
              <div class="min-w-0">
                <dt class="text-xs text-muted-foreground">{m.vendor_plugins_actual_version()}</dt>
                <dd class="font-technical mt-1 truncate tabular-nums">{plugin.version}</dd>
              </div>
              <div class="min-w-0">
                <dt class="text-xs text-muted-foreground">{m.vendor_plugins_source_label()}</dt>
                <dd class="mt-1"><Badge variant="outline">{sourceLabel(plugin.source)}</Badge></dd>
              </div>
              {#if plugin.builtin_version}
                <div class="min-w-0">
                  <dt class="text-xs text-muted-foreground">{m.vendor_plugins_bundled_version()}</dt>
                  <dd class="font-technical mt-1 truncate tabular-nums">{plugin.builtin_version}</dd>
                </div>
              {/if}
            </dl>
            <p class="text-xs text-muted-foreground">
              {m.vendor_plugins_card_capability_count({ count: plugin.capabilities.length })} · {m.vendor_plugins_card_binding_count(
                { count: plugin.affected_bindings.length },
              )}
            </p>

            {#if plugin.pending_update}
              <Alert.Root variant="warning">
                <CircleAlertIcon />
                <Alert.Title>{m.vendor_plugins_pending_builtin_update()}</Alert.Title>
                <Alert.Description>
                  {plugin.pending_update.previous_version
                    ? m.vendor_plugins_preview_version_change({
                        previous: plugin.pending_update.previous_version,
                        next: plugin.pending_update.new_version,
                      })
                    : m.vendor_plugins_preview_new_install({ version: plugin.pending_update.new_version })}
                </Alert.Description>
              </Alert.Root>
            {/if}

            {#if plugin.error}
              <Alert.Root variant="destructive">
                <CircleAlertIcon />
                <Alert.Title>{m.vendor_plugins_reported_error()}</Alert.Title>
                <Alert.Description><p class="line-clamp-3 break-words">{plugin.error}</p></Alert.Description>
              </Alert.Root>
            {/if}

            {#if restoreError?.vendorId === plugin.vendor_id}
              <RequestFailure message={restoreError.message} />
            {/if}
          </Card.Content>
          <Card.Footer class="flex flex-wrap justify-end gap-2 border-t">
            <Button variant="ghost" size="sm" onclick={() => openDetails(plugin)}>
              {m.vendor_plugins_details()}
            </Button>
            {#if canRestoreBuiltin(plugin)}
              <Button
                variant="outline"
                size="sm"
                disabled={restoringVendorId === plugin.vendor_id}
                aria-busy={restoringVendorId === plugin.vendor_id}
                onclick={() => restoreBuiltin(plugin.vendor_id)}>
                {#if restoringVendorId === plugin.vendor_id}<Spinner data-icon="inline-start" />{/if}
                {restoringVendorId === plugin.vendor_id
                  ? m.vendor_plugins_restoring()
                  : m.vendor_plugins_restore_builtin()}
              </Button>
            {/if}
            {#if canUninstall(plugin)}
              <Button variant="outline" size="sm" onclick={() => openUninstall(plugin)}>
                {m.vendor_plugins_uninstall_action()}
              </Button>
            {/if}
            {#if plugin.pending_update}
              {@const pendingUpdate = plugin.pending_update}
              <Button size="sm" onclick={() => showPreview(pendingUpdate)}>
                {m.vendor_plugins_review_update()}
              </Button>
            {/if}
          </Card.Footer>
        </Card.Root>
      {/each}

      <button
        type="button"
        class="flex min-h-44 flex-col items-center justify-center gap-2 rounded-xl border border-dashed border-input p-6 text-center text-muted-foreground transition-colors hover:border-primary/60 hover:text-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50"
        onclick={pickPluginFile}
        disabled={importing}>
        <span class="flex size-9 items-center justify-center rounded-lg border border-border">
          {#if importing}<Spinner class="size-4" />{:else}<UploadIcon class="size-4" />{/if}
        </span>
        <span class="font-medium">{m.vendor_plugins_import_title()}</span>
        <span class="max-w-64 text-xs text-balance">{m.vendor_plugins_import_tile_description()}</span>
      </button>
    </div>
  {/if}
</div>

<Sheet.Root bind:open={detailOpen}>
  <Sheet.Content side="right" class="w-full! gap-0 sm:max-w-md!" closeLabel={m.common_close()}>
    {#if detailPlugin}
      {@const plugin = detailPlugin}
      <Sheet.Header class="pr-12">
        <Sheet.Title class="flex items-center gap-2.5">
          <span
            class="font-structural flex size-8 shrink-0 items-center justify-center rounded-lg bg-accent text-xs font-semibold text-accent-foreground"
            aria-hidden="true">{plugin.name.charAt(0).toUpperCase()}</span>
          <span class="truncate">{plugin.name}</span>
        </Sheet.Title>
        <Sheet.Description class="font-technical">{plugin.vendor_id}</Sheet.Description>
      </Sheet.Header>
      <div class="route-overlay-body">
        <div class="flex flex-col gap-5">
          <dl class="grid grid-cols-2 gap-x-4 gap-y-3 text-sm">
            <div class="min-w-0">
              <dt class="text-xs text-muted-foreground">{m.common_status()}</dt>
              <dd class="mt-1">
                <StatusIndicator label={statusLabel(plugin.status)} tone={statusTone(plugin.status, plugin.error)} />
              </dd>
            </div>
            <div class="min-w-0">
              <dt class="text-xs text-muted-foreground">{m.vendor_plugins_actual_version()}</dt>
              <dd class="font-technical mt-1 truncate tabular-nums">{plugin.version}</dd>
            </div>
            <div class="min-w-0">
              <dt class="text-xs text-muted-foreground">{m.vendor_plugins_source_label()}</dt>
              <dd class="mt-1"><Badge variant="outline">{sourceLabel(plugin.source)}</Badge></dd>
            </div>
            {#if plugin.builtin_version}
              <div class="min-w-0">
                <dt class="text-xs text-muted-foreground">{m.vendor_plugins_bundled_version()}</dt>
                <dd class="font-technical mt-1 truncate tabular-nums">{plugin.builtin_version}</dd>
              </div>
            {/if}
          </dl>

          {#if plugin.pending_update}
            <Alert.Root variant="warning">
              <CircleAlertIcon />
              <Alert.Title>{m.vendor_plugins_pending_builtin_update()}</Alert.Title>
              <Alert.Description>
                {plugin.pending_update.previous_version
                  ? m.vendor_plugins_preview_version_change({
                      previous: plugin.pending_update.previous_version,
                      next: plugin.pending_update.new_version,
                    })
                  : m.vendor_plugins_preview_new_install({ version: plugin.pending_update.new_version })}
              </Alert.Description>
            </Alert.Root>
          {/if}

          {#if plugin.error}
            <Alert.Root variant="destructive">
              <CircleAlertIcon />
              <Alert.Title>{m.vendor_plugins_reported_error()}</Alert.Title>
              <Alert.Description><p class="break-words">{plugin.error}</p></Alert.Description>
            </Alert.Root>
          {/if}

          {#if restoreError?.vendorId === plugin.vendor_id}
            <RequestFailure message={restoreError.message} />
          {/if}

          <section aria-labelledby="detail-capabilities">
            <h3 id="detail-capabilities" class="text-xs font-medium text-muted-foreground">
              {m.vendor_plugins_capabilities()}
            </h3>
            {#if plugin.capabilities.length > 0}
              <div class="mt-2 flex flex-wrap gap-1.5">
                {#each plugin.capabilities as capability (capability)}
                  <Badge variant="secondary" class="font-technical">{capability}</Badge>
                {/each}
              </div>
            {:else}
              <p class="mt-1 text-sm text-muted-foreground">{m.vendor_plugins_no_capabilities()}</p>
            {/if}
          </section>

          <section aria-labelledby="detail-bindings">
            <h3 id="detail-bindings" class="text-xs font-medium text-muted-foreground">
              {m.vendor_plugins_bindings()}
            </h3>
            {#if plugin.affected_bindings.length > 0}
              <ul class="mt-2 flex flex-col gap-2">
                {#each plugin.affected_bindings as binding (`${binding.route_id}:${binding.provider_id}:${binding.capability}:${binding.upstream_model ?? ''}`)}
                  <li class="min-w-0 rounded-lg border bg-card px-3 py-2 text-sm">
                    <span class="font-technical block truncate text-xs">{bindingLabel(binding)}</span>
                    <span class="mt-1 block text-muted-foreground">{binding.capability}</span>
                  </li>
                {/each}
              </ul>
            {:else}
              <p class="mt-1 text-sm text-muted-foreground">{m.vendor_plugins_no_bindings()}</p>
            {/if}
          </section>
        </div>
      </div>
      {#if plugin.pending_update || canRestoreBuiltin(plugin) || canUninstall(plugin)}
        <Sheet.Footer class="border-t">
          {#if canRestoreBuiltin(plugin)}
            <Button
              variant="outline"
              size="sm"
              disabled={restoringVendorId === plugin.vendor_id}
              aria-busy={restoringVendorId === plugin.vendor_id}
              onclick={() => restoreBuiltin(plugin.vendor_id)}>
              {#if restoringVendorId === plugin.vendor_id}<Spinner data-icon="inline-start" />{/if}
              {restoringVendorId === plugin.vendor_id
                ? m.vendor_plugins_restoring()
                : m.vendor_plugins_restore_builtin()}
            </Button>
          {/if}
          {#if canUninstall(plugin)}
            <Button variant="outline" size="sm" onclick={() => openUninstall(plugin)}>
              {m.vendor_plugins_uninstall_action()}
            </Button>
          {/if}
          {#if plugin.pending_update}
            {@const pendingUpdate = plugin.pending_update}
            <Button size="sm" onclick={() => showPreview(pendingUpdate)}>
              {m.vendor_plugins_review_update()}
            </Button>
          {/if}
        </Sheet.Footer>
      {/if}
    {/if}
  </Sheet.Content>
</Sheet.Root>

<AlertDialog.Root bind:open={() => uninstallOpen, setUninstallOpen}>
  {#if uninstallTarget}
    <AlertDialog.Content class="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-lg">
      <AlertDialog.Header>
        <AlertDialog.Title>
          {m.vendor_plugins_uninstall_title({ name: uninstallTarget.name })}
        </AlertDialog.Title>
        <AlertDialog.Description>{m.vendor_plugins_uninstall_description()}</AlertDialog.Description>
      </AlertDialog.Header>

      <div class="flex flex-col gap-3">
        <Alert.Root>
          <Alert.Title>{m.vendor_plugins_uninstall_retained_title()}</Alert.Title>
          <Alert.Description>{m.vendor_plugins_uninstall_retained_description()}</Alert.Description>
        </Alert.Root>
        <Alert.Root variant="warning">
          <CircleAlertIcon />
          <Alert.Title>{m.vendor_plugins_uninstall_interruptions_title()}</Alert.Title>
          <Alert.Description>{m.vendor_plugins_uninstall_interruptions_description()}</Alert.Description>
        </Alert.Root>
        {#if uninstallError}
          <RequestFailure title={m.vendor_plugins_uninstall_failed()} message={uninstallError} />
        {/if}
      </div>

      <AlertDialog.Footer>
        <AlertDialog.Cancel disabled={uninstalling}>{m.common_cancel()}</AlertDialog.Cancel>
        <AlertDialog.Action
          variant="destructive"
          disabled={uninstalling}
          aria-busy={uninstalling}
          onclick={(event: MouseEvent) => {
            event.preventDefault()
            void uninstallPlugin()
          }}>
          {#if uninstalling}<Spinner data-icon="inline-start" />{/if}
          {uninstalling ? m.vendor_plugins_uninstalling() : m.vendor_plugins_uninstall_confirm()}
        </AlertDialog.Action>
      </AlertDialog.Footer>
    </AlertDialog.Content>
  {/if}
</AlertDialog.Root>

<Dialog.Root bind:open={() => previewOpen, setPreviewOpen}>
  {#if preview}
    <Dialog.Layout class="sm:max-w-xl" showCloseButton={!confirming}>
      {#snippet header()}
        <Dialog.Title>{m.vendor_plugins_preview_title()}</Dialog.Title>
        <Dialog.Description>{m.vendor_plugins_preview_description()}</Dialog.Description>
      {/snippet}

      <div class="flex flex-col gap-4">
        <div class="min-w-0">
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
            <h2 class="truncate font-medium">{preview.name}</h2>
            <span class="font-technical text-xs text-muted-foreground">{preview.vendor_id}</span>
            <span class="font-technical ms-auto text-sm tabular-nums">
              {preview.previous_version
                ? m.vendor_plugins_preview_version_change({
                    previous: preview.previous_version,
                    next: preview.new_version,
                  })
                : m.vendor_plugins_preview_new_install({ version: preview.new_version })}
            </span>
          </div>
          <div class="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1">
            <Badge variant="outline">{sourceLabel(preview.target_source)}</Badge>
            {#if preview.is_downgrade}
              <Badge variant="destructive">{m.vendor_plugins_preview_downgrade()}</Badge>
            {/if}
            <span class="text-xs text-muted-foreground">
              {m.vendor_plugins_preview_author()}: {preview.author ?? m.vendor_plugins_preview_author_unknown()}
            </span>
          </div>
          <p class="mt-1.5 text-xs text-muted-foreground">{m.vendor_plugins_preview_author_unverified()}</p>
        </div>

        <div class="grid grid-cols-3 gap-2">
          <div class={cn('rounded-lg border p-3', networkChanges > 0 && 'border-warning/50')}>
            <p class="text-xs text-muted-foreground">{m.vendor_plugins_preview_impact_network()}</p>
            <p
              class={cn(
                'font-structural mt-1 text-base font-semibold tabular-nums',
                networkChanges > 0 && 'text-warning',
              )}>
              {networkTileValue}
            </p>
          </div>
          <div class={cn('rounded-lg border p-3', dataDiscardRequired && 'border-destructive/50')}>
            <p class="text-xs text-muted-foreground">{m.vendor_plugins_preview_impact_data()}</p>
            <p
              class={cn(
                'font-structural mt-1 text-base font-semibold tabular-nums',
                dataDiscardRequired && 'text-destructive',
              )}>
              {dataDiscardRequired
                ? m.vendor_plugins_preview_impact_data_count({ count: discardKindCount })
                : m.vendor_plugins_preview_impact_none()}
            </p>
          </div>
          <div
            class={cn(
              'rounded-lg border p-3',
              (preview.affected_bindings.length > 0 || sessionsCancelled) && 'border-warning/50',
            )}>
            <p class="text-xs text-muted-foreground">{m.vendor_plugins_preview_impact_scope()}</p>
            <p
              class={cn(
                'font-structural mt-1 text-base font-semibold',
                (preview.affected_bindings.length > 0 || sessionsCancelled) && 'text-warning',
              )}>
              {m.vendor_plugins_preview_scope_value({
                bindings: preview.affected_bindings.length,
                sessions: sessionsCancelled ? preview.affected_auth_sessions : 0,
              })}
            </p>
          </div>
        </div>

        {#if hasRisks}
          <section class="rounded-lg border border-warning/50 p-3" aria-labelledby="preview-risks-title">
            <h3 id="preview-risks-title" class="text-sm font-medium">
              {m.vendor_plugins_preview_risks_title()}
            </h3>
            <ul class="mt-2 flex flex-col gap-2.5 text-sm">
              {#each preview.discarded_data as discard (discard.provider.id)}
                <li class="min-w-0">
                  <p class="break-words">
                    <span class="font-medium">{discard.provider.name}</span>
                    <span class="font-technical text-xs text-muted-foreground"> · {discard.provider.id}</span>
                  </p>
                  <p class="mt-0.5 text-xs break-words text-muted-foreground">{discard.kinds.join(' · ')}</p>
                  {#if discard.recovery_actions.length > 0}
                    <p class="mt-0.5 text-xs break-words text-muted-foreground">
                      {m.vendor_plugins_preview_recovery_hint({ actions: discard.recovery_actions.join(' · ') })}
                    </p>
                  {/if}
                </li>
              {/each}
              {#if addedPermissions.length > 0}
                <li class="min-w-0">
                  <p class="font-medium">{m.vendor_plugins_preview_network_added_list()}</p>
                  <ul class="mt-1 flex flex-col gap-1">
                    {#each addedPermissions as permission (`${permission.origin}:${permission.provider_id ?? ''}:${permission.configuration_field ?? ''}`)}
                      {@const context = networkPermissionContext(permission)}
                      <li class="min-w-0">
                        <span class="font-technical text-xs break-all">{permission.origin}</span>
                        {#if context.length > 0}
                          <span class="text-xs text-muted-foreground"> · {context.join(' · ')}</span>
                        {/if}
                      </li>
                    {/each}
                  </ul>
                </li>
              {/if}
              {#if removedPermissions.length > 0}
                <li class="min-w-0">
                  <p class="font-medium">{m.vendor_plugins_preview_network_removed_list()}</p>
                  <ul class="mt-1 flex flex-col gap-1">
                    {#each removedPermissions as origin (origin)}
                      <li class="font-technical min-w-0 text-xs break-all">{origin}</li>
                    {/each}
                  </ul>
                </li>
              {/if}
              {#if preview.affected_bindings.length > 0}
                <li class="min-w-0">
                  <p class="font-medium">
                    {m.vendor_plugins_preview_binding_lost({ count: preview.affected_bindings.length })}
                  </p>
                  <ul class="mt-1 flex flex-col gap-1">
                    {#each preview.affected_bindings as binding (`${binding.route_id}:${binding.provider_id}:${binding.capability}:${binding.upstream_model ?? ''}`)}
                      <li class="min-w-0 text-xs">
                        <span class="font-technical break-words">{bindingLabel(binding)}</span>
                        <span class="text-muted-foreground"> · {binding.capability}</span>
                      </li>
                    {/each}
                  </ul>
                  <p class="mt-1 text-xs text-muted-foreground">{m.vendor_plugins_preview_binding_warning()}</p>
                </li>
              {/if}
              {#if preview.cancels_active_operations}
                <li class="min-w-0">
                  <p class="font-medium">
                    {m.vendor_plugins_preview_operations_cancelled({ count: preview.active_operations })}
                  </p>
                  <p class="mt-0.5 text-xs text-muted-foreground">
                    {m.vendor_plugins_preview_cancellation_limit()}
                  </p>
                </li>
              {/if}
              {#if sessionsCancelled}
                <li class="min-w-0">
                  <p class="font-medium">
                    {m.vendor_plugins_preview_auth_sessions_cancelled({ count: preview.affected_auth_sessions })}
                  </p>
                </li>
              {/if}
              {#if preview.affected_providers.length > 0 && !preview.inherits_credentials}
                <li class="min-w-0">
                  <p class="font-medium">{m.vendor_plugins_preview_credentials_not_inherited()}</p>
                </li>
              {/if}
            </ul>
          </section>
        {/if}

        {#if safeNotes.length > 0}
          <p class="text-xs text-muted-foreground">
            <span class="font-medium text-foreground">{m.vendor_plugins_preview_unaffected_label()}</span>
            · {safeNotes.join(' · ')}
          </p>
        {/if}

        {#if dataDiscardRequired}
          <div class="flex items-start gap-3 rounded-lg border border-warning/40 p-3">
            <Checkbox id="allow-plugin-data-discard" bind:checked={allowDataDiscard} disabled={confirming} />
            <Label for="allow-plugin-data-discard" class="leading-5 font-normal">
              {m.vendor_plugins_preview_allow_data_discard()}
            </Label>
          </div>
        {/if}

        {#if confirmError}
          <RequestFailure title={m.vendor_plugins_preview_failed()} message={confirmError} />
        {/if}
      </div>

      {#snippet footer()}
        <Button variant="outline" disabled={confirming} onclick={() => setPreviewOpen(false)}
          >{m.common_cancel()}</Button>
        <Button
          disabled={confirming || (dataDiscardRequired && !allowDataDiscard)}
          aria-busy={confirming}
          onclick={confirmPreview}>
          {#if confirming}<Spinner data-icon="inline-start" />{/if}
          {confirming ? m.vendor_plugins_preview_confirming() : m.vendor_plugins_preview_confirm()}
        </Button>
      {/snippet}
    </Dialog.Layout>
  {/if}
</Dialog.Root>
