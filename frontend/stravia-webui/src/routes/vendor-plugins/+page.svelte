<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import type { ComponentProps } from 'svelte'
import { onDestroy, onMount } from 'svelte'
import PluginProgressNotification from './PluginProgressNotification.svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import CircleAlertIcon from '@lucide/svelte/icons/circle-alert'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import PuzzleIcon from '@lucide/svelte/icons/puzzle'
import UploadIcon from '@lucide/svelte/icons/upload'
import { toast } from 'svelte-sonner'

import { admin } from '#lib/admin-client.js'
import { marketplaceStatus } from '#lib/marketplace-status.js'
import { openExternalUrl } from '#lib/open-external.js'
import type { MarketplaceEntry, MarketplaceRelease, MarketplaceProgress, PluginSourceSummary } from '#lib/types.js'
import PluginSources from './PluginSources.svelte'
import SearchCombobox from '#lib/components/search-combobox.svelte'
import { Input } from '#lib/components/ui/input/index.js'
import { localizeBackendErrorMessage, localizeMarketplaceError } from '#lib/backend-error.js'
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
} from '#lib/components/ui/data-table/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js'
import { Label } from '#lib/components/ui/label/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import * as Tabs from '#lib/components/ui/tabs/index.js'
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
let previewPreviousSource = $state<PluginSource>()
let previewOpen = $state(false)
let confirmError = $state<string>()
let confirming = $state(false)
let allowDataDiscard = $state(false)
let uninstallTarget = $state<PluginSummary>()
let uninstallOpen = $state(false)
let uninstalling = $state(false)
let uninstallError = $state<string>()
let detailOpen = $state(false)
let detailContent = $state<HTMLElement | null>(null)
let previewCancel = $state<HTMLButtonElement | null>(null)

// 默认初始焦点会落在首个 TechnicalValue 的 Tooltip 触发器上：脚本聚焦会弹出气泡遮住内容，并让第一次 Escape 只关气泡。
function focusDetailClose(event: Event): void {
  event.preventDefault()
  requestAnimationFrame(() => detailContent?.querySelector<HTMLElement>('[data-slot="sheet-close"]')?.focus())
}

function focusPreviewCancel(event: Event): void {
  event.preventDefault()
  requestAnimationFrame(() => previewCancel?.focus())
}

type MarketRow = {
  key: string
  vendorId: string
  entry?: MarketplaceEntry
  source?: PluginSourceSummary
  installed?: PluginSummary
}
const marketQuery = createQuery(() => ({ queryKey: ['plugin-marketplace'], queryFn: admin.pluginMarketplace.get }))
let sourceOpen = $state(false)
let search = $state('')
let sourceFilter = $state('')
let tab = $state<'all' | 'installed' | 'updates'>('all')
let detailKey = $state<string>()
let preparing = $state(false)
let progress = $state<MarketplaceProgress>()
let prepareError = $state<string>()
let refreshError = $state<string>()
let refreshing = $state(false)
let abort: AbortController | undefined
let destroyed = false
const progressToastId = 'plugin-install-progress'

onMount(() => {
  void refreshMarket()
})
onDestroy(() => {
  destroyed = true
  abort?.abort()
  toast.dismiss(progressToastId)
})

function showProgress(name: string, stage: string, bytes?: string, onCancel?: () => void): void {
  if (destroyed) return
  toast.custom(PluginProgressNotification, {
    id: progressToastId,
    style: 'pointer-events: auto;',
    duration: Number.POSITIVE_INFINITY,
    position: 'bottom-right',
    componentProps: { name, stage, bytes, onCancel },
  })
}
const plugins = $derived(pluginsQuery.data ?? [])
const sources = $derived(marketQuery.data?.sources ?? [])
const rows = $derived.by(() => {
  const entries = marketQuery.data?.entries ?? []
  const result: MarketRow[] = entries.map((entry) => ({
    key: `${entry.source_id}:${entry.vendor_id}`,
    vendorId: entry.vendor_id,
    entry,
    source: sources.find((source) => source.id === entry.source_id),
    installed: plugins.find((plugin) => plugin.vendor_id === entry.vendor_id),
  }))
  for (const installed of plugins) {
    if (
      !result.some(
        (row) => row.vendorId === installed.vendor_id && row.entry?.source_id === installed.origin?.source_id,
      )
    ) {
      result.push({
        key: `installed:${installed.vendor_id}`,
        vendorId: installed.vendor_id,
        installed,
        source: sources.find((source) => source.id === installed.origin?.source_id),
      })
    }
  }
  return result
})
const filteredRows = $derived(
  rows.filter((row) => {
    const name =
      row.entry?.latest?.display_name ?? row.entry?.releases[0]?.display_name ?? row.installed?.name ?? row.vendorId
    const attached = attachedVersion(row) !== undefined
    return (
      (!search || `${name} ${row.vendorId}`.toLowerCase().includes(search.toLowerCase())) &&
      (!sourceFilter || row.entry?.source_id === sourceFilter) &&
      (tab === 'all' || (tab === 'installed' && attached) || (tab === 'updates' && row.entry?.update && attached))
    )
  }),
)
const detailRow = $derived(rows.find((row) => row.key === detailKey))
function rowName(row: MarketRow): string {
  return row.entry?.latest?.display_name ?? row.entry?.releases[0]?.display_name ?? row.installed?.name ?? row.vendorId
}
function rowRelease(row: MarketRow): MarketplaceRelease | undefined {
  return row.entry?.update ?? row.entry?.latest ?? row.entry?.releases[0] ?? undefined
}
/** 只有来自本行来源（或无来源条目）的安装才算本行已安装版本。 */
function attachedVersion(row: MarketRow): string | undefined {
  return row.installed && (!row.entry || row.installed.origin?.source_id === row.entry.source_id)
    ? row.installed.version
    : undefined
}
type StatusTone = NonNullable<ComponentProps<typeof StatusIndicator>['tone']>
type RowStatus = { label: string; tone: StatusTone; primary: boolean }
function rowStatus(row: MarketRow): RowStatus {
  switch (marketplaceStatus(row.entry, row.source, row.installed)) {
    case 'conflict':
      return { label: m.market_conflict(), tone: 'error', primary: false }
    case 'source-error':
      return { label: m.market_source_error(), tone: 'warning', primary: false }
    case 'source-disabled':
      return { label: m.market_source_disabled(), tone: 'neutral', primary: false }
    case 'source-removed':
      return { label: m.market_source_removed(), tone: 'warning', primary: false }
    case 'update':
      return { label: m.market_update_available(), tone: 'warning', primary: true }
    case 'host-upgrade':
      return { label: m.market_host_upgrade(), tone: 'warning', primary: false }
    case 'latest':
      return { label: m.market_latest(), tone: 'healthy', primary: false }
    case 'installed':
      return { label: m.market_installed(), tone: 'healthy', primary: false }
    default:
      // 只有新安装与更新是行内主任务；重装、替换来源等低频动作降为次级按钮。
      return { label: m.market_available(), tone: 'neutral', primary: true }
  }
}
function actionLabel(row: MarketRow): string {
  if (!row.installed) return m.market_install()
  if (!row.installed.origin) return m.market_reinstall()
  if (row.installed.origin.source_id === row.entry?.source_id)
    return row.entry?.update ? m.market_update() : m.market_reinstall()
  return m.market_replace_source()
}
async function refreshMarket(): Promise<void> {
  if (refreshing) return
  refreshing = true
  refreshError = undefined
  try {
    await admin.pluginMarketplace.refresh({})
  } catch (error) {
    refreshError = localizeMarketplaceError(error)
  } finally {
    try {
      await queryClient.invalidateQueries({ queryKey: ['plugin-marketplace'] })
    } catch (error) {
      refreshError ??= localizeMarketplaceError(error)
    } finally {
      refreshing = false
    }
  }
}
async function prepare(row: MarketRow, release: MarketplaceRelease): Promise<void> {
  if (!row.entry || preparing) return
  const controller = new AbortController()
  detailOpen = false
  preparing = true
  prepareError = undefined
  progress = undefined
  abort = controller
  const name = rowName(row)
  const cancel = () => controller.abort()
  showProgress(name, progressLabel(), undefined, cancel)
  try {
    const result = await admin.pluginMarketplace.prepareInstall(
      {
        source_id: row.entry.source_id,
        release_id: release.release_id,
        vendor_id: row.vendorId,
        version: release.version,
        catalog_sha256: release.catalog_sha256,
      },
      (value) => {
        if (controller.signal.aborted || destroyed) return
        progress = value
        const bytes =
          value.stage === 'download' && value.received !== undefined
            ? value.total
              ? `${formatSize(value.received)} / ${formatSize(value.total)}`
              : formatSize(value.received)
            : undefined
        showProgress(name, progressLabel(), bytes, cancel)
      },
      controller.signal,
    )
    if (!controller.signal.aborted && !destroyed) {
      showPreview(result)
    }
  } catch (error) {
    if (!controller.signal.aborted && !destroyed) prepareError = localizeMarketplaceError(error)
  } finally {
    if (!destroyed) toast.dismiss(progressToastId)
    preparing = false
    abort = undefined
  }
}
function progressLabel(): string {
  switch (progress?.stage) {
    case 'download':
      return progress.total
        ? m.market_downloading_percent({ percent: Math.round(((progress.received ?? 0) * 100) / progress.total) })
        : m.market_downloading()
    case 'verify':
      return m.market_verifying()
    case 'decompress':
      return m.market_decompressing()
    case 'admission':
      return m.market_admission()
    default:
      return m.market_preparing()
  }
}

function formatSize(bytes: number): string {
  return bytes >= 1024 * 1024 ? `${(bytes / (1024 * 1024)).toFixed(1)} MiB` : `${Math.ceil(bytes / 1024)} KiB`
}
const marketColumnHelper = createDataTableColumnHelper<MarketRow>()
const marketColumns = marketColumnHelper.columns([
  marketColumnHelper.display({
    id: 'name',
    header: () => m.common_name(),
    cell: (context) => renderSnippet(marketIdentity, context),
    meta: { label: () => m.common_name(), width: 'fill' },
    size: 220,
  }),
  marketColumnHelper.display({
    id: 'source',
    header: () => m.vendor_plugins_source_label(),
    cell: (context) => renderSnippet(marketSource, context),
    meta: { label: () => m.vendor_plugins_source_label(), width: 'compact' },
    size: 160,
  }),
  marketColumnHelper.display({
    id: 'installed',
    header: () => m.market_installed(),
    cell: (context) => renderSnippet(marketInstalled, context),
    meta: { label: () => m.market_installed(), width: 'compact' },
    size: 88,
  }),
  marketColumnHelper.display({
    id: 'available',
    header: () => m.market_available(),
    cell: (context) => renderSnippet(marketAvailable, context),
    meta: { label: () => m.market_available(), width: 'compact' },
    size: 128,
  }),
  marketColumnHelper.display({
    id: 'status',
    header: () => m.common_status(),
    cell: (context) => renderSnippet(marketStatus, context),
    meta: { label: () => m.common_status(), width: 'compact' },
    size: 150,
  }),
  marketColumnHelper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(marketActions, context),
    meta: { label: () => m.common_actions(), width: 'compact', align: 'end' },
    size: 200,
  }),
])
const fileDragActive = $derived(fileDragDepth > 0)
const importBusy = $derived(importing || previewOpen || importQueue.length > 0)
const tableLabels = $derived(getDataTableLabels())
const dataDiscardRequired = $derived((preview?.discarded_data.length ?? 0) > 0)
// 同源更新不重复显示「原来源」；只有跨来源替换或从随附/本地迁出时才需要提示。
const previewSourceChanged = $derived(
  preview?.previous_origin
    ? preview.previous_origin.source_id !== preview.origin?.source_id
    : previewPreviousSource !== undefined && previewPreviousSource !== preview?.target_source,
)
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
  return source === 'builtin'
    ? m.vendor_plugins_source_builtin()
    : source === 'marketplace'
      ? m.plugin_sources_title()
      : m.vendor_plugins_source_local()
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
    (plugin.source !== 'builtin' || plugin.status !== 'ready' || plugin.version !== plugin.builtin_version),
  )
}

function canUninstall(plugin: PluginSummary): boolean {
  return plugin.vendor_id !== 'base'
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

function showPreview(value: PluginPreview): void {
  preview = value
  previewPreviousSource = value.previous_version
    ? plugins.find((plugin) => plugin.vendor_id === value.vendor_id)?.source
    : undefined
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
    queryClient.invalidateQueries({ queryKey: ['plugin-marketplace'] }),
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
  showProgress(`${current.name} · ${current.new_version}`, m.vendor_plugins_preview_confirming())
  try {
    await admin.vendorPlugins.confirm({
      preview_id: current.id,
      allow_data_discard: dataDiscardRequired ? allowDataDiscard : false,
    })
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['vendor-plugins'] }),
      queryClient.invalidateQueries({ queryKey: ['plugin-marketplace'] }),
    ])
    if (!destroyed) {
      previewOpen = false
      preview = undefined
      toast.success(m.vendor_plugins_update_applied())
      advanceImportQueue()
    }
  } catch (error) {
    if (!destroyed) confirmError = localizeBackendErrorMessage(error)
  } finally {
    if (!destroyed) toast.dismiss(progressToastId)
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

{#snippet marketIdentity(context: DataTableCellContext<MarketRow>)}
  {@const row = context.row.original}
  <div class="flex min-w-0 items-center gap-3">
    <ProviderMark
      class="market-icon size-9 shrink-0"
      name={rowName(row)}
      svg={row.entry?.latest?.icon_svg ?? row.installed?.icon_svg}
      logo={row.vendorId} />
    <span class="truncate font-medium">{rowName(row)}</span>{#if row.source && !row.source.official}<PuzzleIcon
        class="size-4 shrink-0 text-muted-foreground"
        aria-label={m.market_external()} />{/if}
  </div>
{/snippet}
{#snippet marketSource(context: DataTableCellContext<MarketRow>)}
  {@const row = context.row.original}
  <span class="flex min-w-0 items-center gap-2">
    {#if row.source}<span class="truncate">{row.source.repository}</span>
      {#if row.source.official}<Badge variant="outline" class="shrink-0">{m.plugin_sources_official()}</Badge>{/if}
    {:else if row.installed?.origin}<span class="truncate text-muted-foreground"
        >{row.installed.origin.repository}</span>
    {:else if row.installed}<span class="text-muted-foreground">{sourceLabel(row.installed.source)}</span>{/if}
  </span>
{/snippet}
{#snippet marketInstalled(context: DataTableCellContext<MarketRow>)}
  {@const version = attachedVersion(context.row.original)}
  {#if version}<span class="font-technical tabular-nums">{version}</span>{:else}<span class="text-muted-foreground"
      >—</span
    >{/if}
{/snippet}
{#snippet marketAvailable(context: DataTableCellContext<MarketRow>)}
  {@const release = rowRelease(context.row.original)}
  {#if release}<span class="font-technical tabular-nums">{release.version}</span>
    <span class="ms-1.5 text-xs text-muted-foreground tabular-nums">{formatSize(release.artifact.size)}</span>
  {:else}<span class="text-muted-foreground">—</span>{/if}
{/snippet}
{#snippet marketStatus(context: DataTableCellContext<MarketRow>)}
  {@render statusCell(context.row.original)}
{/snippet}
{#snippet statusCell(row: MarketRow)}
  {@const status = rowStatus(row)}
  {@const release = rowRelease(row)}
  <div class="min-w-0">
    <StatusIndicator compact label={status.label} tone={status.tone} />
    {#if release && !release.compatible}<p class="mt-0.5 text-xs break-words text-muted-foreground">
        {release.incompatibility ?? release.compatibility.host_version_req}
      </p>{/if}
  </div>
{/snippet}
{#snippet marketActions(context: DataTableCellContext<MarketRow>)}
  {@render rowActions(context.row.original, 'justify-end')}
{/snippet}
{#snippet rowActions(row: MarketRow, className: string)}
  {@const release = rowRelease(row)}
  <div class={cn('flex items-center gap-2 whitespace-nowrap', className)}>
    {#if row.entry && release && row.vendorId !== 'base'}
      <Button
        size="sm"
        variant={rowStatus(row).primary ? 'default' : 'outline'}
        disabled={preparing || !row.source?.enabled || release.conflict || !release.compatible || !!row.source?.error}
        onclick={() => prepare(row, release)}>{actionLabel(row)}</Button>
    {/if}
    <Button
      size="sm"
      variant="ghost"
      onclick={() => {
        detailKey = row.key
        detailOpen = true
      }}>{m.vendor_plugins_details()}</Button>
  </div>
{/snippet}
{#snippet pageActions()}
  <Button variant="outline" disabled={refreshing} onclick={refreshMarket}
    >{#if refreshing}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
        data-icon="inline-start" />{/if}{m.market_check_updates()}</Button>
  <Button variant="outline" onclick={() => (sourceOpen = true)}>{m.plugin_sources_title()}</Button>
  <DropdownMenu.Root>
    <DropdownMenu.Trigger>
      {#snippet child({ props })}
        <Button {...props} variant="outline" size="icon" aria-label={m.common_more_actions()}
          ><MoreHorizontalIcon /></Button>
      {/snippet}
    </DropdownMenu.Trigger>
    <DropdownMenu.Content class="w-48" align="end">
      <DropdownMenu.Item disabled={importBusy} onSelect={pickPluginFile}
        ><UploadIcon />{m.vendor_plugins_import_title()}</DropdownMenu.Item>
    </DropdownMenu.Content>
  </DropdownMenu.Root>
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
  <PageHeader title={m.vendor_plugins_title()} actions={pageActions} />
  <input
    bind:this={fileInput}
    bind:files={selectedFiles}
    id="vendor-plugin-file"
    type="file"
    accept=".wasm,.wasm.zst,application/wasm,application/zstd"
    multiple
    class="hidden"
    onchange={() => importPluginFiles(Array.from(selectedFiles ?? []))} />
  {#if pluginsQuery.error}<RequestFailure
      title={m.vendor_plugins_load_failed()}
      message={localizeBackendErrorMessage(pluginsQuery.error)}
      retry={() => pluginsQuery.refetch()} />{/if}
  {#if marketQuery.error}<RequestFailure
      message={localizeBackendErrorMessage(marketQuery.error)}
      retry={() => marketQuery.refetch()} />{/if}
  {#if refreshError || importError || prepareError}<Alert.Root variant="destructive"
      ><CircleAlertIcon /><Alert.Description>{refreshError ?? importError ?? prepareError}</Alert.Description
      ></Alert.Root
    >{/if}
  <section class="route-section vendor-plugins-workspace">
    <Tabs.Root bind:value={tab}>
      <Tabs.List aria-label={m.vendor_plugins_title()}>
        <Tabs.Trigger value="all">{m.market_all()}</Tabs.Trigger>
        <Tabs.Trigger value="installed">{m.market_installed()}</Tabs.Trigger>
        <Tabs.Trigger value="updates"
          >{m.market_updates()} <Badge variant="outline">{marketQuery.data?.update_count ?? 0}</Badge></Tabs.Trigger>
      </Tabs.List>
    </Tabs.Root>
    <div class="flex flex-wrap gap-2">
      <Input
        class="min-w-0 flex-[999_1_16rem]"
        aria-label={m.market_search()}
        placeholder={m.market_search()}
        bind:value={search} /><SearchCombobox
        id="marketplace-source-filter"
        class="max-w-80 min-w-0 flex-[1_1_14rem]"
        value={sourceFilter}
        options={sources.map((source) => ({ value: source.id, label: source.repository }))}
        placeholder={m.market_all_sources()}
        emptyText={m.market_no_results()}
        clearLabel={m.common_table_clear_filter()}
        onValueChange={(value: string) => (sourceFilter = value)} />
    </div>
    {#each sources.filter((source) => source.error || !source.succeeded_at) as source (source.id)}
      <p class="flex items-center gap-1.5 text-sm text-warning">
        <CircleAlertIcon class="size-4 shrink-0" aria-hidden="true" />{source.repository}: {source.error
          ? m.market_cached_error()
          : m.market_not_checked()}{#if source.succeeded_at}
          — {m.plugin_sources_last_success({ date: new Date(source.succeeded_at * 1000).toLocaleString() })}{/if}
      </p>
    {/each}
    {#if !marketQuery.isPending && !pluginsQuery.isPending && filteredRows.length === 0}
      <div class="py-10 text-center text-muted-foreground">
        {search || sourceFilter || tab !== 'all'
          ? m.market_no_results()
          : sources.length > 0 && sources.every((source) => source.error)
            ? m.market_all_failed()
            : m.market_empty()}
        <Button variant="link" onclick={refreshMarket}>{m.market_check_updates()}</Button>
      </div>
    {:else}
      <div class="route-desktop-table min-h-0 flex-1">
        <DataTable
          workbench
          data={filteredRows}
          columns={marketColumns}
          labels={tableLabels}
          getRowId={(row: MarketRow) => row.key}
          ariaLabel={m.vendor_plugins_title()}
          loading={marketQuery.isPending || pluginsQuery.isPending} />
      </div>
      <ul class="route-mobile-list border-t">
        {#each filteredRows as row (row.key)}{@const release = rowRelease(row)}
          <li class="flex gap-3 border-b py-3">
            <ProviderMark
              class="market-icon size-9 shrink-0"
              name={rowName(row)}
              svg={row.entry?.latest?.icon_svg ?? row.installed?.icon_svg}
              logo={row.vendorId} />
            <div class="flex min-w-0 flex-1 flex-col gap-2">
              <div class="min-w-0">
                <p class="flex min-w-0 items-center gap-1.5 font-medium">
                  <span class="truncate">{rowName(row)}</span>{#if row.source && !row.source.official}<PuzzleIcon
                      class="size-4 shrink-0 text-muted-foreground"
                      aria-label={m.market_external()} />{/if}
                </p>
                <p class="truncate text-xs text-muted-foreground">
                  {row.source?.repository ??
                    row.installed?.origin?.repository ??
                    (row.installed ? sourceLabel(row.installed.source) : '')}
                </p>
              </div>
              {@render statusCell(row)}
              <dl class="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-0.5 text-xs">
                <dt class="text-muted-foreground">{m.market_installed()}</dt>
                <dd class="font-technical tabular-nums">{attachedVersion(row) ?? '—'}</dd>
                <dt class="text-muted-foreground">{m.market_available()}</dt>
                <dd class="tabular-nums">
                  {#if release}<span class="font-technical">{release.version}</span>
                    <span class="text-muted-foreground"> · {formatSize(release.artifact.size)}</span>{:else}—{/if}
                </dd>
              </dl>
              {@render rowActions(row, 'flex-wrap')}
            </div>
          </li>{/each}
      </ul>
    {/if}
  </section>
  {#if fileDragActive}<div
      class="pointer-events-none absolute inset-0 z-40 flex items-center justify-center border-2 border-dashed border-primary bg-background/90">
      {m.vendor_plugins_drop_hint()}
    </div>{/if}
</div>
<PluginSources
  bind:open={sourceOpen}
  onchanged={() => queryClient.invalidateQueries({ queryKey: ['plugin-marketplace'] })} />
<Sheet.Root bind:open={detailOpen}
  ><Sheet.Content
    bind:ref={detailContent}
    side="right"
    class="w-full! gap-0 sm:max-w-2xl!"
    closeLabel={m.common_close()}
    onOpenAutoFocus={focusDetailClose}>
    {#if detailRow}{@const row = detailRow}{@const release = rowRelease(row)}
      <Sheet.Header><Sheet.Title>{rowName(row)}</Sheet.Title></Sheet.Header>
      <div class="route-overlay-body @container flex flex-col gap-6">
        <div class="flex flex-col gap-3">
          <div class="flex flex-wrap items-start gap-x-4 gap-y-2">
            <TechnicalValue class="text-xs text-muted-foreground" value={row.vendorId} copyable />
            {@render statusCell(row)}
          </div>
          {#if release?.description}<p class="text-pretty">{release.description}</p>{/if}
        </div>
        {#if row.source || release?.authors?.length}<dl class="market-detail-list">
            {#if row.source}
              <dt>{m.plugin_sources_repository()}</dt>
              <dd class="flex min-w-0 flex-wrap items-center gap-2">
                <button
                  type="button"
                  class="min-w-0 truncate text-primary underline-offset-4 hover:underline"
                  onclick={() => void openExternalUrl(row.source!.url)}>{row.source.repository}</button>
                {#if row.source.official}<Badge variant="outline">{m.plugin_sources_official()}</Badge>{/if}
              </dd>
              <dt>{m.plugin_sources_fingerprint()}</dt>
              <dd class="min-w-0"><TechnicalValue value={row.source.fingerprint} copyable /></dd>
            {/if}
            {#if release?.authors?.length}
              <dt>{m.market_authors()}</dt>
              <dd>{release.authors.join(', ')}</dd>
            {/if}
          </dl>{/if}
        {#if row.installed}<section class="flex flex-col gap-3" aria-labelledby="market-detail-installed">
            <h3 id="market-detail-installed" class="text-sm font-semibold">{m.market_installed()}</h3>
            <dl class="market-detail-list">
              <dt>{m.vendor_plugins_actual_version()}</dt>
              <dd class="font-technical tabular-nums">{row.installed.version}</dd>
              <dt>{m.vendor_plugins_source_label()}</dt>
              <dd class="min-w-0">
                {#if row.installed.origin}<span class="break-words">{row.installed.origin.repository}</span> ·
                  <button
                    type="button"
                    class="font-technical text-primary underline-offset-4 hover:underline"
                    onclick={() => void openExternalUrl(row.installed!.origin!.release_url)}
                    >{row.installed.origin.release_tag}</button
                  >{:else}{sourceLabel(row.installed.source)}{/if}
                {#if row.installed.source === 'local'}<p class="text-xs text-muted-foreground">
                    {m.market_local_unverified()}
                  </p>{/if}
              </dd>
              {#if row.installed.origin}
                <dt>{m.plugin_sources_fingerprint()}</dt>
                <dd class="min-w-0">
                  <TechnicalValue value={row.installed.origin.public_key_fingerprint} copyable />
                </dd>
              {/if}
              {#if row.installed.digest}
                <dt>{m.market_component_digest()}</dt>
                <dd class="min-w-0"><TechnicalValue value={row.installed.digest} copyable /></dd>
              {/if}
              {#if row.installed.installed_at}
                <dt>{m.market_installed_at()}</dt>
                <dd class="font-technical tabular-nums">
                  {new Date(row.installed.installed_at * 1000).toLocaleString()}
                </dd>
              {/if}
            </dl>
          </section>{/if}
        {#if row.entry}<section class="flex flex-col gap-3" aria-labelledby="market-detail-versions">
            <h3 id="market-detail-versions" class="text-sm font-semibold">{m.market_versions()}</h3>
            {#if row.entry.incomplete}<p class="text-sm text-warning">{m.market_incomplete()}</p>{/if}
            {#if row.entry.conflict}<p class="text-sm text-warning">{m.market_conflict()}</p>{/if}
            <ul class="divide-y border-y">
              {#each row.entry.releases as version (`${version.release_id}:${version.version}`)}<li
                  class="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 py-2.5">
                  <div class="min-w-0">
                    <button
                      type="button"
                      class="font-technical text-primary tabular-nums underline-offset-4 hover:underline"
                      onclick={() => void openExternalUrl(version.release_url)}>{version.version}</button>
                    <span class="text-xs text-muted-foreground tabular-nums">
                      · {new Date(version.published_at).toLocaleDateString()} · {formatSize(version.artifact.size)}
                    </span>
                    {#if version.conflict}<p class="text-xs text-warning">{m.market_conflict()}</p>{/if}
                    {#if !version.compatible}<p class="text-xs text-warning">
                        {m.market_host_upgrade()}: {version.incompatibility ?? version.compatibility.host_version_req}
                      </p>{/if}
                  </div>
                  {#if row.vendorId !== 'base'}<Button
                      size="sm"
                      variant="outline"
                      disabled={preparing ||
                        !row.source?.enabled ||
                        !!row.source?.error ||
                        version.conflict ||
                        !version.compatible}
                      onclick={() => prepare(row, version)}>{m.market_install_version()}</Button
                    >{/if}
                </li>{/each}
            </ul>
          </section>{/if}
        {#if row.installed?.error}<RequestFailure message={row.installed.error} />{/if}
        {#if restoreError?.vendorId === row.vendorId}<RequestFailure message={restoreError.message} />{/if}
      </div>
      <Sheet.Footer class="border-t"
        >{#if row.installed?.pending_update}<Button onclick={() => showPreview(row.installed!.pending_update!)}
            >{m.vendor_plugins_review_update()}</Button
          >{/if}{#if row.installed && canRestoreBuiltin(row.installed)}<Button
            variant="outline"
            disabled={restoringVendorId === row.vendorId}
            onclick={() => restoreBuiltin(row.vendorId)}>{m.vendor_plugins_restore_builtin()}</Button
          >{/if}{#if row.installed && canUninstall(row.installed)}<Button
            variant="outline"
            onclick={() => openUninstall(row.installed!)}>{m.vendor_plugins_uninstall_action()}</Button
          >{/if}</Sheet.Footer>
    {/if}
  </Sheet.Content></Sheet.Root>

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
    <Dialog.Layout class="sm:max-w-xl" showCloseButton={!confirming} onOpenAutoFocus={focusPreviewCancel}>
      {#snippet header()}
        <Dialog.Title>{m.vendor_plugins_preview_title()}</Dialog.Title>
      {/snippet}

      <div class="@container flex flex-col gap-4">
        <div class="flex min-w-0 flex-col gap-3">
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
            <ProviderMark
              class="size-7 rounded-md text-xs"
              name={preview.name}
              svg={preview.icon_svg}
              logo={preview.vendor_id} />
            <div class="min-w-0">
              <h2 class="truncate font-medium">{preview.name}</h2>
              <p class="font-technical truncate text-xs text-muted-foreground">{preview.vendor_id}</p>
            </div>
            <span class="ms-auto flex items-center gap-2 text-sm tabular-nums">
              {#if preview.is_downgrade}<Badge variant="destructive">{m.vendor_plugins_preview_downgrade()}</Badge>{/if}
              {preview.previous_version
                ? m.vendor_plugins_preview_version_change({
                    previous: preview.previous_version,
                    next: preview.new_version,
                  })
                : m.vendor_plugins_preview_new_install({ version: preview.new_version })}
            </span>
          </div>
          <dl class="market-detail-list">
            <dt>{m.vendor_plugins_source_label()}</dt>
            <dd class="min-w-0">
              {#if preview.origin}<span class="break-words">{preview.origin.repository}</span> ·
                <button
                  type="button"
                  class="font-technical text-primary underline-offset-4 hover:underline"
                  onclick={() => void openExternalUrl(preview!.origin!.release_url)}
                  >{preview.origin.release_tag}</button
                >{:else}{sourceLabel(preview.target_source)}{/if}
            </dd>
            {#if preview.origin}
              <dt>{m.plugin_sources_fingerprint()}</dt>
              <dd class="min-w-0">
                <TechnicalValue value={preview.origin.public_key_fingerprint} copyable />
              </dd>
            {/if}
            {#if previewSourceChanged}
              <dt>{m.market_previous_source()}</dt>
              <dd class="min-w-0 break-words">
                {preview.previous_origin?.repository ??
                  (previewPreviousSource ? sourceLabel(previewPreviousSource) : '')}
              </dd>
            {/if}
            <dt>{m.vendor_plugins_preview_author()}</dt>
            <dd>
              {preview.author ?? m.vendor_plugins_preview_author_unknown()}
              <p class="text-xs text-muted-foreground">{m.vendor_plugins_preview_author_unverified()}</p>
            </dd>
          </dl>
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
        <Button bind:ref={previewCancel} variant="outline" disabled={confirming} onclick={() => setPreviewOpen(false)}
          >{m.common_cancel()}</Button>
        <Button
          disabled={confirming || (dataDiscardRequired && !allowDataDiscard)}
          aria-busy={confirming}
          onclick={confirmPreview}>
          {#if confirming}<Spinner data-icon="inline-start" />{/if}
          {confirming
            ? m.vendor_plugins_preview_confirming()
            : preview?.origin
              ? m.market_confirm_install({ name: preview.name, version: preview.new_version })
              : m.vendor_plugins_preview_confirm()}
        </Button>
      {/snippet}
    </Dialog.Layout>
  {/if}
</Dialog.Root>

<style>
:global(.market-icon img) {
  filter: grayscale(1);
}
/* 详情里的标签与值成对排列；窄 Sheet 时改为上下堆叠，避免长指纹挤压标签列。 */
.market-detail-list {
  display: grid;
  grid-template-columns: minmax(7rem, max-content) minmax(0, 1fr);
  gap: 0.5rem 1rem;
  align-items: baseline;
  font-size: 0.8125rem;
}
.market-detail-list :global(dt) {
  color: var(--muted-foreground);
  font-size: 0.75rem;
}
@container (max-width: 26rem) {
  .market-detail-list {
    grid-template-columns: minmax(0, 1fr);
    gap: 0.125rem;
  }
  .market-detail-list :global(dd) {
    margin-bottom: 0.5rem;
  }
}
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
