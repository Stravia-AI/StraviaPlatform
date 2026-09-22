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
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import { Label } from '$lib/components/ui/label'
import { Skeleton } from '$lib/components/ui/skeleton'
import { Spinner } from '$lib/components/ui/spinner'

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
let previewKind = $state<'import' | 'pending' | 'restore'>()
let previewOpen = $state(false)
let confirmError = $state<string>()
let confirming = $state(false)
let allowDataDiscard = $state(false)
let uninstallTarget = $state<PluginSummary>()
let uninstallOpen = $state(false)
let uninstalling = $state(false)
let uninstallError = $state<string>()

const plugins = $derived(pluginsQuery.data ?? [])
const selectedFile = $derived(selectedFiles?.item(0) ?? undefined)
const dataDiscardRequired = $derived((preview?.discarded_data.length ?? 0) > 0)
const networkChanges = $derived(
  (preview?.network_permissions.length ?? 0) + (preview?.removed_network_permissions.length ?? 0),
)

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

function showPreview(value: PluginPreview, kind: 'import' | 'pending' | 'restore'): void {
  preview = value
  previewKind = kind
  allowDataDiscard = false
  confirmError = undefined
  previewOpen = true
}

async function importPlugin(): Promise<void> {
  const file = selectedFile
  importError = undefined
  if (!file) {
    importError = m.vendor_plugins_file_required()
    return
  }
  if (file.size > MAX_PLUGIN_BYTES) {
    importError = m.vendor_plugins_file_too_large()
    return
  }

  importing = true
  try {
    showPreview(await admin.vendorPlugins.import(file), 'import')
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
    showPreview(await admin.vendorPlugins.restoreBuiltin(vendorId), 'restore')
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
    if (previewKind === 'import') {
      selectedFiles = undefined
      if (fileInput) fileInput.value = ''
    }
    previewKind = undefined
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
    description={m.vendor_plugins_summary()} />

  <section class="route-section" aria-labelledby="plugin-import-title">
    <div class="route-section-header">
      <div>
        <h2 id="plugin-import-title" class="route-section-title">{m.vendor_plugins_import_title()}</h2>
        <p class="route-section-description">{m.vendor_plugins_import_description()}</p>
      </div>
    </div>

    <Field.Group class="max-w-3xl">
      <Field.Field data-invalid={Boolean(importError)} orientation="vertical" size="fill">
        <Field.Label for="vendor-plugin-file">{m.vendor_plugins_file_label()}</Field.Label>
        <Field.Content>
          <Input
            bind:ref={fileInput}
            bind:files={selectedFiles}
            id="vendor-plugin-file"
            type="file"
            accept=".wasm,application/wasm"
            aria-invalid={Boolean(importError)}
            disabled={importing}
            onchange={() => (importError = undefined)} />
          <Field.Description>{m.vendor_plugins_file_help()}</Field.Description>
          {#if importError}<Field.Error>{importError}</Field.Error>{/if}
        </Field.Content>
      </Field.Field>
      <div class="flex flex-wrap gap-2">
        <Button onclick={importPlugin} disabled={importing} aria-busy={importing}>
          {#if importing}<Spinner data-icon="inline-start" />{:else}<UploadIcon data-icon="inline-start" />{/if}
          {importing ? m.vendor_plugins_importing() : m.vendor_plugins_import_action()}
        </Button>
      </div>
    </Field.Group>
  </section>

  <section class="route-section" aria-labelledby="plugin-list-title">
    <div class="route-section-header">
      <div>
        <h2 id="plugin-list-title" class="route-section-title">{m.vendor_plugins_list_title()}</h2>
        <p class="route-section-description">{m.vendor_plugins_list_description()}</p>
      </div>
      <Button
        variant="outline"
        size="sm"
        disabled={pluginsQuery.isFetching}
        aria-label={m.vendor_plugins_refresh()}
        onclick={() => pluginsQuery.refetch()}>
        {#if pluginsQuery.isFetching}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
            data-icon="inline-start" />{/if}
        {m.vendor_plugins_refresh()}
      </Button>
    </div>

    {#if pluginsQuery.error}
      <RequestFailure
        class="mb-4"
        title={m.vendor_plugins_load_failed()}
        message={localizeBackendErrorMessage(pluginsQuery.error)}
        retry={() => pluginsQuery.refetch()}
        retrying={pluginsQuery.isFetching} />
    {/if}

    {#if pluginsQuery.isPending}
      <div class="grid gap-4 lg:grid-cols-2" aria-busy="true">
        {#each Array(4) as _, index (index)}
          <Card.Root>
            <Card.Header><Skeleton class="h-5 w-40" /><Skeleton class="h-4 w-56" /></Card.Header>
            <Card.Content class="flex flex-col gap-3"><Skeleton class="h-16" /><Skeleton class="h-8" /></Card.Content>
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
      </Empty.Root>
    {:else if plugins.length > 0}
      <div class="grid items-start gap-4 xl:grid-cols-2">
        {#each plugins as plugin (plugin.vendor_id)}
          {@const canRestoreBuiltin =
            plugin.builtin_version &&
            !plugin.pending_update &&
            (plugin.source === 'local' || plugin.status !== 'ready' || plugin.version !== plugin.builtin_version)}
          {@const canUninstall = plugin.vendor_id !== 'base'}
          <Card.Root>
            <Card.Header>
              <Card.Title class="min-w-0 truncate">{plugin.name}</Card.Title>
              <Card.Description class="font-technical truncate">{plugin.vendor_id}</Card.Description>
              <Card.Action>
                <StatusIndicator label={statusLabel(plugin.status)} tone={statusTone(plugin.status, plugin.error)} />
              </Card.Action>
            </Card.Header>
            <Card.Content class="flex flex-col gap-4">
              <dl class="grid grid-cols-2 gap-x-4 gap-y-3 text-sm sm:grid-cols-3">
                <div class="min-w-0">
                  <dt class="text-xs text-muted-foreground">{m.vendor_plugins_actual_version()}</dt>
                  <dd class="font-technical mt-1 truncate tabular-nums">{plugin.version}</dd>
                </div>
                <div class="min-w-0">
                  <dt class="text-xs text-muted-foreground">{m.vendor_plugins_preview_target_source()}</dt>
                  <dd class="mt-1"><Badge variant="outline">{sourceLabel(plugin.source)}</Badge></dd>
                </div>
                {#if plugin.builtin_version}
                  <div class="min-w-0">
                    <dt class="text-xs text-muted-foreground">{m.vendor_plugins_bundled_version()}</dt>
                    <dd class="font-technical mt-1 truncate tabular-nums">{plugin.builtin_version}</dd>
                  </div>
                {/if}
              </dl>

              <div>
                <h3 class="text-xs font-medium text-muted-foreground">{m.vendor_plugins_capabilities()}</h3>
                {#if plugin.capabilities.length > 0}
                  <div class="mt-2 flex flex-wrap gap-1.5">
                    {#each plugin.capabilities as capability (capability)}
                      <Badge variant="secondary" class="font-technical">{capability}</Badge>
                    {/each}
                  </div>
                {:else}
                  <p class="mt-1 text-sm text-muted-foreground">{m.vendor_plugins_no_capabilities()}</p>
                {/if}
              </div>

              <div>
                <h3 class="text-xs font-medium text-muted-foreground">{m.vendor_plugins_bindings()}</h3>
                {#if plugin.affected_bindings.length > 0}
                  <ul class="mt-2 flex flex-col gap-2">
                    {#each plugin.affected_bindings as binding (`${binding.route_id}:${binding.provider_id}:${binding.capability}:${binding.upstream_model ?? ''}`)}
                      <li class="min-w-0 text-sm">
                        <span class="font-technical block truncate text-xs">{bindingLabel(binding)}</span>
                        <span class="text-muted-foreground">{binding.capability}</span>
                      </li>
                    {/each}
                  </ul>
                {:else}
                  <p class="mt-1 text-sm text-muted-foreground">{m.vendor_plugins_no_bindings()}</p>
                {/if}
              </div>

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
            </Card.Content>
            {#if plugin.pending_update || canRestoreBuiltin || canUninstall}
              <Card.Footer class="flex flex-wrap justify-end gap-2 border-t">
                {#if canRestoreBuiltin}
                  <Button
                    variant="outline"
                    disabled={restoringVendorId === plugin.vendor_id}
                    aria-busy={restoringVendorId === plugin.vendor_id}
                    onclick={() => restoreBuiltin(plugin.vendor_id)}>
                    {#if restoringVendorId === plugin.vendor_id}<Spinner data-icon="inline-start" />{/if}
                    {restoringVendorId === plugin.vendor_id
                      ? m.vendor_plugins_restoring()
                      : m.vendor_plugins_restore_builtin()}
                  </Button>
                {/if}
                {#if plugin.pending_update}
                  {@const pendingUpdate = plugin.pending_update}
                  <Button onclick={() => showPreview(pendingUpdate, 'pending')}>
                    {m.vendor_plugins_review_update()}
                  </Button>
                {/if}
                {#if canUninstall}
                  <Button variant="outline" onclick={() => openUninstall(plugin)}>
                    {m.vendor_plugins_uninstall_action()}
                  </Button>
                {/if}
              </Card.Footer>
            {/if}
          </Card.Root>
        {/each}
      </div>
    {/if}
  </section>
</div>

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

<Dialog.Root bind:open={previewOpen}>
  {#if preview}
    <Dialog.Layout class="sm:max-w-3xl" showCloseButton={!confirming}>
      {#snippet header()}
        <Dialog.Title>{m.vendor_plugins_preview_title()}</Dialog.Title>
        <Dialog.Description>{m.vendor_plugins_preview_description()}</Dialog.Description>
      {/snippet}

      <div class="flex flex-col gap-5">
        <section aria-labelledby="preview-identity-title">
          <div class="flex flex-wrap items-start justify-between gap-3">
            <div class="min-w-0">
              <h2 id="preview-identity-title" class="route-section-title">{preview.name}</h2>
              <p class="font-technical mt-1 truncate text-xs text-muted-foreground">{preview.vendor_id}</p>
            </div>
            <div class="flex flex-wrap gap-1.5">
              <Badge variant="outline">{sourceLabel(preview.target_source)}</Badge>
              {#if preview.is_downgrade}<Badge variant="destructive">{m.vendor_plugins_preview_downgrade()}</Badge>{/if}
            </div>
          </div>
          <p class="font-technical mt-3 text-sm tabular-nums">
            {preview.previous_version
              ? m.vendor_plugins_preview_version_change({
                  previous: preview.previous_version,
                  next: preview.new_version,
                })
              : m.vendor_plugins_preview_new_install({ version: preview.new_version })}
          </p>
          <dl class="mt-3 grid gap-3 sm:grid-cols-2">
            <div>
              <dt class="text-xs text-muted-foreground">{m.vendor_plugins_preview_author()}</dt>
              <dd class="mt-1 font-medium">{preview.author ?? m.vendor_plugins_preview_author_unknown()}</dd>
            </div>
            <div>
              <dt class="text-xs text-muted-foreground">{m.vendor_plugins_preview_target_source()}</dt>
              <dd class="mt-1 font-medium">{sourceLabel(preview.target_source)}</dd>
            </div>
          </dl>
          <p class="mt-2 text-xs text-muted-foreground">{m.vendor_plugins_preview_author_unverified()}</p>
        </section>

        <section class="route-section" aria-labelledby="preview-connections-title">
          <h2 id="preview-connections-title" class="route-section-title">
            {m.vendor_plugins_preview_connections_title()}
          </h2>
          <p class="route-section-description">{m.vendor_plugins_preview_connections_description()}</p>
          {#if preview.affected_providers.length > 0}
            <ul class="mt-3 grid gap-2 sm:grid-cols-2">
              {#each preview.affected_providers as provider (provider.id)}
                <li class="min-w-0 rounded-lg border bg-card px-3 py-2">
                  <span class="block truncate font-medium">{provider.name}</span>
                  <span class="font-technical block truncate text-xs text-muted-foreground">{provider.id}</span>
                </li>
              {/each}
            </ul>
          {:else}
            <p class="mt-3 text-sm text-muted-foreground">{m.vendor_plugins_preview_no_connections()}</p>
          {/if}
          {#if preview.affected_providers.length > 0}
            <Alert.Root class="mt-3" variant={preview.inherits_credentials ? 'default' : 'warning'}>
              <Alert.Description>
                {preview.inherits_credentials
                  ? m.vendor_plugins_preview_credentials_inherited()
                  : m.vendor_plugins_preview_credentials_not_inherited()}
              </Alert.Description>
            </Alert.Root>
          {/if}
        </section>

        <section class="route-section" aria-labelledby="preview-network-title">
          <h2 id="preview-network-title" class="route-section-title">{m.vendor_plugins_preview_network_title()}</h2>
          <p class="route-section-description">{m.vendor_plugins_preview_network_description()}</p>
          {#if networkChanges === 0}
            <p class="mt-3 text-sm text-muted-foreground">{m.vendor_plugins_preview_network_none()}</p>
          {:else}
            <ul class="mt-3 flex flex-col gap-2">
              {#each preview.network_permissions as permission (`${permission.origin}:${permission.provider_id ?? ''}:${permission.configuration_field ?? ''}`)}
                {@const context = networkPermissionContext(permission)}
                <li class="flex min-w-0 items-start gap-2 rounded-lg border bg-card px-3 py-2">
                  <Badge variant={permission.added ? 'default' : 'outline'}>
                    {permission.added
                      ? m.vendor_plugins_preview_network_added()
                      : m.vendor_plugins_preview_network_existing()}
                  </Badge>
                  <div class="min-w-0">
                    <span class="font-technical block break-all text-xs">{permission.origin}</span>
                    {#if context.length > 0}
                      <span class="mt-1 block text-xs text-muted-foreground">{context.join(' · ')}</span>
                    {/if}
                  </div>
                </li>
              {/each}
              {#each preview.removed_network_permissions as origin (origin)}
                <li class="flex min-w-0 items-start gap-2 rounded-lg border bg-card px-3 py-2">
                  <Badge variant="destructive">{m.vendor_plugins_preview_network_removed()}</Badge>
                  <span class="font-technical min-w-0 break-all text-xs">{origin}</span>
                </li>
              {/each}
            </ul>
          {/if}
        </section>

        <section class="route-section" aria-labelledby="preview-data-title">
          <h2 id="preview-data-title" class="route-section-title">{m.vendor_plugins_preview_data_title()}</h2>
          {#if preview.discarded_data.length === 0}
            <p class="mt-3 text-sm text-muted-foreground">{m.vendor_plugins_preview_data_compatible()}</p>
          {:else}
            <Alert.Root variant="warning" class="mt-3">
              <CircleAlertIcon />
              <Alert.Description>{m.vendor_plugins_preview_data_warning()}</Alert.Description>
            </Alert.Root>
            <div class="mt-3 flex flex-col gap-3">
              {#each preview.discarded_data as discard (discard.provider.id)}
                <article class="rounded-lg border bg-card p-3">
                  <h3 class="font-medium">{discard.provider.name}</h3>
                  <p class="font-technical mt-0.5 text-xs text-muted-foreground">{discard.provider.id}</p>
                  <div class="mt-3 grid gap-3 sm:grid-cols-2">
                    <div>
                      <h4 class="text-xs font-medium text-muted-foreground">
                        {m.vendor_plugins_preview_data_kinds()}
                      </h4>
                      <ul class="mt-1 flex list-disc flex-col gap-1 pl-4 text-sm">
                        {#each discard.kinds as kind (kind)}<li>{kind}</li>{/each}
                      </ul>
                    </div>
                    <div>
                      <h4 class="text-xs font-medium text-muted-foreground">
                        {m.vendor_plugins_preview_recovery_actions()}
                      </h4>
                      <ul class="mt-1 flex list-disc flex-col gap-1 pl-4 text-sm">
                        {#each discard.recovery_actions as action (action)}<li>{action}</li>{/each}
                      </ul>
                    </div>
                  </div>
                </article>
              {/each}
            </div>
            <div class="mt-4 flex items-start gap-3 rounded-lg border border-warning/40 p-3">
              <Checkbox id="allow-plugin-data-discard" bind:checked={allowDataDiscard} disabled={confirming} />
              <Label for="allow-plugin-data-discard" class="leading-5 font-normal">
                {m.vendor_plugins_preview_allow_data_discard()}
              </Label>
            </div>
          {/if}
        </section>

        <section class="route-section" aria-labelledby="preview-bindings-title">
          <h2 id="preview-bindings-title" class="route-section-title">
            {m.vendor_plugins_preview_binding_title()}
          </h2>
          {#if preview.affected_bindings.length === 0}
            <p class="mt-3 text-sm text-muted-foreground">{m.vendor_plugins_preview_binding_none()}</p>
          {:else}
            <Alert.Root variant="warning" class="mt-3">
              <CircleAlertIcon />
              <Alert.Description>{m.vendor_plugins_preview_binding_warning()}</Alert.Description>
            </Alert.Root>
            <ul class="mt-3 flex flex-col gap-2">
              {#each preview.affected_bindings as binding (`${binding.route_id}:${binding.provider_id}:${binding.capability}:${binding.upstream_model ?? ''}`)}
                <li class="rounded-lg border bg-card px-3 py-2 text-sm">
                  <span class="font-technical block break-words text-xs">{bindingLabel(binding)}</span>
                  <span class="mt-1 block text-muted-foreground">{binding.capability}</span>
                </li>
              {/each}
            </ul>
          {/if}
        </section>

        <section class="route-section" aria-labelledby="preview-operations-title">
          <h2 id="preview-operations-title" class="route-section-title">
            {m.vendor_plugins_preview_operations_title()}
          </h2>
          {#if preview.cancels_active_operations}
            <Alert.Root variant="warning" class="mt-3">
              <CircleAlertIcon />
              <Alert.Description>
                <p>{m.vendor_plugins_preview_operations_cancelled({ count: preview.active_operations })}</p>
                <p>{m.vendor_plugins_preview_cancellation_limit()}</p>
              </Alert.Description>
            </Alert.Root>
          {:else}
            <p class="mt-3 text-sm text-muted-foreground">{m.vendor_plugins_preview_operations_continue()}</p>
          {/if}
          <div class="mt-3 border-t pt-3">
            <h3 class="text-sm font-medium">{m.vendor_plugins_preview_auth_sessions_title()}</h3>
            <p class="mt-1 text-sm text-muted-foreground">
              {preview.cancels_active_operations && preview.affected_auth_sessions > 0
                ? m.vendor_plugins_preview_auth_sessions_cancelled({ count: preview.affected_auth_sessions })
                : m.vendor_plugins_preview_auth_sessions_none()}
            </p>
          </div>
        </section>

        {#if confirmError}
          <RequestFailure title={m.vendor_plugins_preview_failed()} message={confirmError} />
        {/if}
      </div>

      {#snippet footer()}
        <Button variant="outline" disabled={confirming} onclick={() => (previewOpen = false)}
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
