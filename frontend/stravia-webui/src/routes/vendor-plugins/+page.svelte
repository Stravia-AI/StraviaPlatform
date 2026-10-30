<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import CircleAlertIcon from '@lucide/svelte/icons/circle-alert'
import PackageOpenIcon from '@lucide/svelte/icons/package-open'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import UploadIcon from '@lucide/svelte/icons/upload'
import { toast } from 'svelte-sonner'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import type {
  PluginBindingImpact,
  PluginNetworkPermission,
  PluginPreview,
  PluginSource,
  PluginSummary,
} from '#lib/types.js'
import PageHeader from '#lib/components/page-header.svelte'
import ProviderMark from '#lib/components/provider-mark.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Checkbox } from '#lib/components/ui/checkbox/index.js'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRowPointerEvent,
} from '#lib/components/ui/data-table/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Label } from '#lib/components/ui/label/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { cn } from '#lib/utils.js'

const MAX_PLUGIN_BYTES = 64 * 1024 * 1024
const queryClient = useQueryClient()
const pluginsQuery = createQuery(() => ({ queryKey: ['vendor-plugins'], queryFn: admin.vendorPlugins.list }))

let fileInput = $state<HTMLInputElement | null>(null)
let selectedFiles = $state<FileList>()
let fileDragDepth = $state(0)
let importQueue = $state<File[]>([])
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
const listEmpty = $derived(!pluginsQuery.isPending && !pluginsQuery.error && plugins.length === 0)
const fileDragActive = $derived(fileDragDepth > 0)
const importBusy = $derived(importing || previewOpen || importQueue.length > 0)
const detailPlugin = $derived(plugins.find((plugin) => plugin.vendor_id === detailVendorId))
const pendingCount = $derived(plugins.filter((plugin) => plugin.pending_update).length)
const failedCount = $derived(plugins.filter((plugin) => plugin.error || /error|fail/i.test(plugin.status)).length)
const attentionNotes = $derived(
  [
    pendingCount > 0 ? m.vendor_plugins_attention_pending({ count: pendingCount }) : undefined,
    failedCount > 0 ? m.vendor_plugins_attention_errors({ count: failedCount }) : undefined,
  ].filter((note) => note !== undefined),
)
const attentionVendorId = $derived(
  plugins.find((plugin) => plugin.pending_update || plugin.error || /error|fail/i.test(plugin.status))?.vendor_id,
)
const tableLabels = $derived(getDataTableLabels())
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

// 按用户关心程度排序；未知标识排在最后并原样显示。
const CAPABILITY_ORDER = [
  'infer',
  'search',
  'media_image',
  'compact',
  'model_discovery',
  'auth_oauth',
  'allowance',
] as const
// 插件端配置校验是宿主内部握手，对用户没有独立含义。
const HIDDEN_CAPABILITIES = new Set(['config_validation'])

function listedCapabilities(capabilities: string[]): string[] {
  const rank = (capability: string) => {
    const index = CAPABILITY_ORDER.indexOf(capability as (typeof CAPABILITY_ORDER)[number])
    return index === -1 ? CAPABILITY_ORDER.length : index
  }
  return capabilities.filter((capability) => !HIDDEN_CAPABILITIES.has(capability)).sort((a, b) => rank(a) - rank(b))
}

function capabilityLabel(capability: string): string {
  switch (capability) {
    case 'infer':
      return m.vendor_plugins_capability_infer()
    case 'search':
      return m.vendor_plugins_capability_search()
    case 'media_image':
      return m.vendor_plugins_capability_media_image()
    case 'compact':
      return m.vendor_plugins_capability_compact()
    case 'model_discovery':
      return m.vendor_plugins_capability_model_discovery()
    case 'auth_oauth':
      return m.vendor_plugins_capability_auth_oauth()
    case 'allowance':
      return m.vendor_plugins_capability_allowance()
    default:
      return capability
  }
}

function capabilityDescription(capability: string): string | undefined {
  switch (capability) {
    case 'infer':
      return m.vendor_plugins_capability_infer_description()
    case 'search':
      return m.vendor_plugins_capability_search_description()
    case 'media_image':
      return m.vendor_plugins_capability_media_image_description()
    case 'compact':
      return m.vendor_plugins_capability_compact_description()
    case 'model_discovery':
      return m.vendor_plugins_capability_model_discovery_description()
    case 'auth_oauth':
      return m.vendor_plugins_capability_auth_oauth_description()
    case 'allowance':
      return m.vendor_plugins_capability_allowance_description()
    default:
      return undefined
  }
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
  return m.vendor_plugins_binding_model_value({
    route: binding.route_id,
    provider: binding.provider_id,
    model: binding.upstream_model,
  })
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

const pluginColumnHelper = createDataTableColumnHelper<PluginSummary>()
const pluginColumns = pluginColumnHelper.columns([
  pluginColumnHelper.accessor((plugin) => plugin.name, {
    id: 'name',
    header: () => m.common_name(),
    cell: (context) => renderSnippet(pluginIdentityCell, context),
    enableSorting: false,
    meta: { label: () => m.common_name(), width: 'fill' },
    size: 260,
  }),
  pluginColumnHelper.accessor((plugin) => plugin.version, {
    id: 'version',
    header: () => m.vendor_plugins_actual_version(),
    cell: (context) => renderSnippet(pluginVersionCell, context),
    enableSorting: false,
    meta: { label: () => m.vendor_plugins_actual_version(), width: 'compact' },
    size: 112,
  }),
  pluginColumnHelper.accessor((plugin) => sourceLabel(plugin.source), {
    id: 'source',
    header: () => m.vendor_plugins_source_label(),
    cell: (context) => renderSnippet(pluginSourceCell, context),
    enableSorting: false,
    meta: { label: () => m.vendor_plugins_source_label(), width: 'compact' },
    size: 96,
  }),
  pluginColumnHelper.accessor((plugin) => listedCapabilities(plugin.capabilities).map(capabilityLabel).join(' · '), {
    id: 'capabilities',
    header: () => m.vendor_plugins_features(),
    cell: (context) => renderSnippet(pluginCapabilitiesCell, context),
    enableSorting: false,
    meta: { label: () => m.vendor_plugins_features(), width: 'fill', cellClass: 'whitespace-normal' },
    size: 160,
  }),
  pluginColumnHelper.accessor((plugin) => statusLabel(plugin.status), {
    id: 'status',
    header: () => m.common_status(),
    cell: (context) => renderSnippet(pluginStatusCell, context),
    enableSorting: false,
    meta: { label: () => m.common_status(), width: 'compact' },
    size: 104,
  }),
  pluginColumnHelper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(pluginActionsCell, context),
    enableSorting: false,
    enableHiding: false,
    meta: { label: () => m.common_actions(), width: 'compact', align: 'end' },
    size: 160,
  }),
])

function hasBundledDifference(plugin: PluginSummary): boolean {
  return Boolean(plugin.builtin_version && plugin.builtin_version !== plugin.version)
}

function pluginVersionChange(update: PluginPreview): string {
  return update.previous_version
    ? m.vendor_plugins_preview_version_change({ previous: update.previous_version, next: update.new_version })
    : m.vendor_plugins_preview_new_install({ version: update.new_version })
}

// 紧凑注意计数只负责定位；原因与恢复动作始终在所属行。
function locateAttentionPlugin(): void {
  const rows = Array.from(document.querySelectorAll<HTMLElement>('[data-plugin-row]')).filter(
    (row) => row.dataset.pluginRow === attentionVendorId,
  )
  const target = rows.find((row) => row.getClientRects().length > 0) ?? rows[0]
  if (!target) return
  target.scrollIntoView({ block: 'center' })
  target.focus({ preventScroll: true })
}

function handlePluginRowClick({ event, original }: DataTableRowPointerEvent<PluginSummary>): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  openDetails(original)
}

function pickPluginFile(): void {
  if (importBusy) return
  importError = undefined
  fileInput?.click()
}

function dragHasFiles(event: DragEvent): boolean {
  return Boolean(event.dataTransfer?.types.includes('Files'))
}

function guardFileDragDefault(event: DragEvent): void {
  if (dragHasFiles(event)) event.preventDefault()
}

function handleFileDragEnter(event: DragEvent): void {
  if (!dragHasFiles(event) || importBusy) return
  fileDragDepth += 1
}

function handleFileDragOver(event: DragEvent): void {
  if (!dragHasFiles(event)) return
  event.preventDefault()
  if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy'
}

function handleFileDragLeave(event: DragEvent): void {
  if (!dragHasFiles(event)) return
  fileDragDepth = Math.max(0, fileDragDepth - 1)
}

function handleFileDrop(event: DragEvent): void {
  if (!dragHasFiles(event)) return
  event.preventDefault()
  fileDragDepth = 0
  importPluginFiles(Array.from(event.dataTransfer?.files ?? []))
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
    advanceImportQueue()
  }
}

function importPluginFiles(files: File[]): void {
  const [first, ...rest] = files
  if (!first || importBusy) return
  importError = undefined
  importQueue = rest
  void importPlugin(first)
}

function advanceImportQueue(): void {
  const [next, ...rest] = importQueue
  importQueue = rest
  if (next) void importPlugin(next)
}

async function importPlugin(file: File | undefined): Promise<void> {
  selectedFiles = undefined
  if (fileInput) fileInput.value = ''
  if (!file || importing) return

  let next: PluginPreview | undefined
  if (file.size > MAX_PLUGIN_BYTES) {
    importError = `${file.name}: ${m.vendor_plugins_file_too_large()}`
  } else {
    importing = true
    try {
      next = await admin.vendorPlugins.import(file)
    } catch (error) {
      importError = `${file.name}: ${localizeBackendErrorMessage(error)}`
    } finally {
      importing = false
    }
  }
  if (next) {
    showPreview(next)
  } else {
    advanceImportQueue()
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
    advanceImportQueue()
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

{#snippet pageActions()}
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

{#snippet pluginIssueNotes(plugin: PluginSummary)}
  {#if plugin.pending_update}
    <p class="mt-1 flex items-start gap-1.5 text-xs whitespace-normal text-warning">
      <CircleAlertIcon class="mt-0.5 size-3.5 shrink-0" />
      <span class="min-w-0 break-words">
        {m.vendor_plugins_pending_builtin_update()} · {pluginVersionChange(plugin.pending_update)}
      </span>
    </p>
  {/if}
  {#if plugin.error}
    <p class="mt-1 flex items-start gap-1.5 text-xs whitespace-normal text-destructive">
      <CircleAlertIcon class="mt-0.5 size-3.5 shrink-0" />
      <span class="min-w-0 break-words">{plugin.error}</span>
    </p>
  {/if}
  {#if restoreError?.vendorId === plugin.vendor_id}
    <p class="mt-1 flex items-start gap-1.5 text-xs whitespace-normal text-destructive" role="alert">
      <CircleAlertIcon class="mt-0.5 size-3.5 shrink-0" />
      <span class="min-w-0 break-words">{m.vendor_plugins_restore_failed()} {restoreError.message}</span>
    </p>
  {/if}
  {#if plugin.affected_bindings.length > 0}
    <p class="mt-1 text-xs text-muted-foreground">
      {m.vendor_plugins_card_binding_count({ count: plugin.affected_bindings.length })}
    </p>
  {/if}
{/snippet}

{#snippet pluginActions(plugin: PluginSummary)}
  {@const restoring = restoringVendorId === plugin.vendor_id}
  {#if plugin.pending_update}
    {@const pendingUpdate = plugin.pending_update}
    <Button size="sm" onclick={() => showPreview(pendingUpdate)}>
      {m.vendor_plugins_review_update()}
    </Button>
  {/if}
  {#if canRestoreBuiltin(plugin)}
    <Button
      variant="outline"
      size="sm"
      disabled={restoring}
      aria-busy={restoring}
      onclick={() => restoreBuiltin(plugin.vendor_id)}>
      {#if restoring}<Spinner data-icon="inline-start" />{/if}
      {restoring ? m.vendor_plugins_restoring() : m.vendor_plugins_restore_builtin()}
    </Button>
  {/if}
  {#if canUninstall(plugin)}
    <Button variant="outline" size="sm" onclick={() => openUninstall(plugin)}>
      {m.vendor_plugins_uninstall_action()}
    </Button>
  {/if}
  <Button variant="ghost" size="sm" onclick={() => openDetails(plugin)}>
    {m.vendor_plugins_details()}
  </Button>
{/snippet}

{#snippet pluginIdentityCell(context: DataTableCellContext<PluginSummary>)}
  {@const plugin = context.row.original}
  <div class="flex min-w-0 items-start gap-3" data-plugin-row={plugin.vendor_id} tabindex="-1">
    <ProviderMark
      class="size-10 shrink-0 rounded-lg text-base"
      name={plugin.name}
      svg={plugin.icon_svg}
      logo={plugin.vendor_id} />
    <div class="min-w-0 flex-1">
      <p class="truncate font-medium">{plugin.name}</p>
      {@render pluginIssueNotes(plugin)}
    </div>
  </div>
{/snippet}

{#snippet pluginVersionCell(context: DataTableCellContext<PluginSummary>)}
  {@const plugin = context.row.original}
  <p class="font-technical tabular-nums">{plugin.version}</p>
  {#if hasBundledDifference(plugin)}
    <p class="mt-0.5 text-xs text-muted-foreground">
      {m.vendor_plugins_bundled_version()}
      <span class="font-technical tabular-nums">{plugin.builtin_version}</span>
    </p>
  {/if}
{/snippet}

{#snippet pluginSourceCell(context: DataTableCellContext<PluginSummary>)}
  <Badge variant="outline">{sourceLabel(context.row.original.source)}</Badge>
{/snippet}

{#snippet pluginCapabilitiesCell(context: DataTableCellContext<PluginSummary>)}
  {@const features = listedCapabilities(context.row.original.capabilities)}
  <p class="text-xs leading-5 text-muted-foreground">
    {features.length > 0 ? features.map(capabilityLabel).join(' · ') : m.vendor_plugins_no_capabilities()}
  </p>
{/snippet}

{#snippet pluginStatusCell(context: DataTableCellContext<PluginSummary>)}
  {@const plugin = context.row.original}
  <StatusIndicator compact label={statusLabel(plugin.status)} tone={statusTone(plugin.status, plugin.error)} />
{/snippet}

{#snippet pluginActionsCell(context: DataTableCellContext<PluginSummary>)}
  <div class="flex flex-wrap items-center justify-end gap-2">{@render pluginActions(context.row.original)}</div>
{/snippet}

<svelte:head><title>{m.vendor_plugins_title()} · Stravia</title></svelte:head>

<svelte:window ondragover={guardFileDragDefault} ondrop={guardFileDragDefault} />

<div
  class="route-page vendor-plugins-page relative"
  role="group"
  aria-label={m.vendor_plugins_title()}
  ondragenter={handleFileDragEnter}
  ondragover={handleFileDragOver}
  ondragleave={handleFileDragLeave}
  ondrop={handleFileDrop}>
  <PageHeader title={m.vendor_plugins_title()} actions={listEmpty ? undefined : pageActions} />

  <input
    bind:this={fileInput}
    bind:files={selectedFiles}
    id="vendor-plugin-file"
    type="file"
    accept=".wasm,application/wasm"
    multiple
    class="hidden"
    onchange={() => importPluginFiles(Array.from(selectedFiles ?? []))} />

  {#if pluginsQuery.error}
    <RequestFailure
      title={m.vendor_plugins_load_failed()}
      message={localizeBackendErrorMessage(pluginsQuery.error)}
      retry={() => pluginsQuery.refetch()}
      retrying={pluginsQuery.isFetching} />
  {/if}

  {#if importError}
    <Alert.Root variant="destructive">
      <CircleAlertIcon />
      <Alert.Description>{importError}</Alert.Description>
    </Alert.Root>
  {/if}

  <section class="route-section vendor-plugins-workspace">
    {#if attentionNotes.length > 0}
      <p class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-warning" data-slot="plugin-attention">
        <span class="flex items-center gap-1.5">
          <CircleAlertIcon class="size-3.5 shrink-0" />
          <span class="font-medium">{m.vendor_plugins_attention_title()}</span>
          <span>{attentionNotes.join(' · ')}</span>
        </span>
        <Button variant="link" size="xs" class="h-auto p-0" onclick={locateAttentionPlugin}>
          {m.vendor_plugins_attention_locate()}
        </Button>
      </p>
    {/if}

    {#if listEmpty}
      <Empty.Root class="mx-auto my-auto w-full max-w-xl py-10">
        <Empty.Header>
          <Empty.Media variant="icon"><PackageOpenIcon /></Empty.Media>
          <Empty.Title class="flex items-center justify-center gap-1.5">
            {m.vendor_plugins_empty_title()}
            <Field.FieldHelp hint={m.vendor_plugins_empty_description()} />
          </Empty.Title>
        </Empty.Header>
        <Empty.Content>
          <Button onclick={pickPluginFile} disabled={importing}>
            <UploadIcon data-icon="inline-start" />
            {m.vendor_plugins_import_title()}
          </Button>
        </Empty.Content>
      </Empty.Root>
    {:else if pluginsQuery.isPending}
      <div class="route-desktop-table" aria-busy="true">
        <DataTable
          data={plugins}
          columns={pluginColumns}
          labels={tableLabels}
          ariaLabel={m.vendor_plugins_title()}
          loading
          loadingRows={3} />
      </div>
      <div class="route-mobile-list border-y" aria-busy="true">
        {#each Array(3) as _, index (index)}
          <div class="flex items-start gap-3 border-b py-4 last:border-b-0">
            <Skeleton class="size-10 shrink-0 rounded-lg" />
            <div class="flex flex-1 flex-col gap-2">
              <Skeleton class="h-5 w-40 max-w-full" />
              <Skeleton class="h-4 w-56 max-w-full" />
            </div>
          </div>
        {/each}
      </div>
    {:else if plugins.length > 0}
      <div class="route-desktop-table min-h-0 flex-1">
        <DataTable
          workbench
          data={plugins}
          columns={pluginColumns}
          labels={tableLabels}
          getRowId={(plugin: PluginSummary) => plugin.vendor_id}
          ariaLabel={m.vendor_plugins_title()}
          onRowClick={handlePluginRowClick} />
      </div>
      <ul class="route-mobile-list border-t">
        {#each plugins as plugin (plugin.vendor_id)}
          {@const features = listedCapabilities(plugin.capabilities)}
          <li class="flex flex-col gap-3 border-b py-4">
            <div class="flex min-w-0 items-start gap-3" data-plugin-row={plugin.vendor_id} tabindex="-1">
              <ProviderMark
                class="size-10 shrink-0 rounded-lg text-base"
                name={plugin.name}
                svg={plugin.icon_svg}
                logo={plugin.vendor_id} />
              <div class="min-w-0 flex-1">
                <p class="font-medium break-words">{plugin.name}</p>
                <p class="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
                  <span class="font-technical tabular-nums" title={m.vendor_plugins_actual_version()}
                    >{plugin.version}</span>
                  <span>{sourceLabel(plugin.source)}</span>
                  {#if hasBundledDifference(plugin)}
                    <span>
                      {m.vendor_plugins_bundled_version()}
                      <span class="font-technical tabular-nums">{plugin.builtin_version}</span>
                    </span>
                  {/if}
                </p>
                {#if features.length > 0}
                  <p class="mt-1 text-xs text-muted-foreground">{features.map(capabilityLabel).join(' · ')}</p>
                {/if}
                {@render pluginIssueNotes(plugin)}
              </div>
            </div>
            <div class="flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
              <StatusIndicator
                compact
                label={statusLabel(plugin.status)}
                tone={statusTone(plugin.status, plugin.error)} />
              <div class="flex flex-wrap justify-end gap-2">{@render pluginActions(plugin)}</div>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  {#if fileDragActive}
    <div class="pointer-events-none absolute inset-0 z-40 bg-background/80">
      <div class="sticky top-0 flex h-[calc(100svh-6rem)] items-center justify-center p-4">
        <div
          class="flex size-full flex-col items-center justify-center gap-2 rounded-xl border-2 border-dashed border-primary text-center">
          <span class="flex size-9 items-center justify-center rounded-lg border border-border bg-background">
            <UploadIcon class="size-4" />
          </span>
          <span class="font-medium">{m.vendor_plugins_import_title()}</span>
          <span class="max-w-64 text-xs text-balance text-muted-foreground">
            {m.vendor_plugins_drop_hint()}
          </span>
        </div>
      </div>
    </div>
  {/if}
</div>

<Sheet.Root bind:open={detailOpen}>
  <Sheet.Content side="right" class="w-full! gap-0 sm:max-w-md!" closeLabel={m.common_close()}>
    {#if detailPlugin}
      {@const plugin = detailPlugin}
      <Sheet.Header class="pr-12">
        <Sheet.Title class="flex items-center gap-2.5">
          <ProviderMark
            class="size-8 rounded-lg text-xs"
            name={plugin.name}
            svg={plugin.icon_svg}
            logo={plugin.vendor_id} />
          <span class="truncate">{plugin.name}</span>
        </Sheet.Title>
      </Sheet.Header>
      <div class="route-overlay-body">
        <div class="flex flex-col gap-5">
          <dl class="grid grid-cols-2 gap-x-4 gap-y-3 text-sm">
            <div class="min-w-0">
              <dt class="text-xs text-muted-foreground">{m.vendor_plugins_id_label()}</dt>
              <dd class="mt-1"><TechnicalValue value={plugin.vendor_id} copyable /></dd>
            </div>
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
            {#if listedCapabilities(plugin.capabilities).length > 0}
              <ul class="mt-2 flex flex-col gap-2">
                {#each listedCapabilities(plugin.capabilities) as capability (capability)}
                  {@const description = capabilityDescription(capability)}
                  <li class="min-w-0 text-sm">
                    <span class="font-medium">{capabilityLabel(capability)}</span>
                    {#if description}<span class="block text-xs text-muted-foreground">{description}</span>{/if}
                  </li>
                {/each}
              </ul>
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
                {#each plugin.affected_bindings as binding (`${binding.route_id}:${binding.provider_id}:${binding.capability}:${binding.upstream_model}`)}
                  <li class="min-w-0 rounded-lg border bg-card px-3 py-2 text-sm">
                    <span class="font-technical block truncate text-xs">{bindingLabel(binding)}</span>
                    <span class="mt-1 block text-muted-foreground">{capabilityLabel(binding.capability)}</span>
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
      {/snippet}

      <div class="flex flex-col gap-4">
        <div class="min-w-0">
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
            <ProviderMark
              class="size-7 rounded-md text-xs"
              name={preview.name}
              svg={preview.icon_svg}
              logo={preview.vendor_id} />
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
                    {#each preview.affected_bindings as binding (`${binding.route_id}:${binding.provider_id}:${binding.capability}:${binding.upstream_model}`)}
                      <li class="min-w-0 text-xs">
                        <span class="font-technical break-words">{bindingLabel(binding)}</span>
                        <span class="text-muted-foreground"> · {capabilityLabel(binding.capability)}</span>
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

        {#if retainedPermissions.length > 0}
          <div class="min-w-0 text-xs" data-slot="plugin-retained-network">
            <p class="font-medium">{m.vendor_plugins_preview_network_retained_list()}</p>
            <ul class="mt-1 flex flex-col gap-1">
              {#each retainedPermissions as permission (`${permission.origin}:${permission.provider_id ?? ''}:${permission.configuration_field ?? ''}`)}
                {@const context = networkPermissionContext(permission)}
                <li class="min-w-0">
                  <span class="font-technical break-all">{permission.origin}</span>
                  {#if context.length > 0}
                    <span class="text-muted-foreground"> · {context.join(' · ')}</span>
                  {/if}
                </li>
              {/each}
            </ul>
          </div>
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

<style>
.vendor-plugins-page {
  /* 与壳层一致，页头与提示按内容占位，其余高度留给资源列表。 */
  height: calc(100svh - 5rem);
}
.vendor-plugins-workspace {
  display: flex;
  flex: 1;
  min-height: 24rem;
  flex-direction: column;
  gap: 0.75rem;
}
@container route-page (max-width: 52rem) {
  .vendor-plugins-workspace {
    flex: none;
    min-height: 0;
  }
}
@media (max-width: 767px) {
  .vendor-plugins-page {
    height: auto;
    min-height: calc(100svh - 4.5rem);
  }
}
</style>
