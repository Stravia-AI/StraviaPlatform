<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import PlusIcon from '@lucide/svelte/icons/plus'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { getDataTableLabels } from '$lib/data-table-labels'
import { formatLogTime } from '$lib/format'
import { effectiveModelDisplayName } from '$lib/logical-model'
import type { ApiKey } from '$lib/types'
import ApiKeyEditor from '$lib/components/api-key-editor.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import StatusIndicator from '$lib/components/status-indicator.svelte'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Button } from '$lib/components/ui/button'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRowPointerEvent,
} from '$lib/components/ui/data-table'
import * as DropdownMenu from '$lib/components/ui/dropdown-menu'
import * as Empty from '$lib/components/ui/empty'
import { Skeleton } from '$lib/components/ui/skeleton'

const queryClient = useQueryClient()
const apiKeysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const webSearchQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const mediaUnderstandingQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))
let editorOpen = $state(false)
let editorApiKey = $state<ApiKey>()
let editorSession = $state(0)
let deleteTarget = $state<ApiKey>()
let deleteOpen = $state(false)
let actingKeyId = $state<string>()

const apiKeys = $derived(apiKeysQuery.data ?? [])
const models = $derived(modelsQuery.data ?? [])
const tableLabels = $derived(getDataTableLabels())
const apiKeyColumnHelper = createDataTableColumnHelper<ApiKey>()
const apiKeyColumns = apiKeyColumnHelper.columns([
  apiKeyColumnHelper.accessor('name', {
    header: () => m.common_name(),
    meta: { label: () => m.common_name(), cellClass: 'font-medium' },
    size: 160,
  }),
  apiKeyColumnHelper.accessor('key', {
    header: () => m.api_keys_masked_secret(),
    cell: (context) => renderSnippet(maskedKeyCell, context),
    meta: { label: () => m.api_keys_masked_secret() },
    size: 170,
  }),
  apiKeyColumnHelper.accessor((apiKey) => limitsLabel(apiKey), {
    id: 'limits',
    header: () => m.api_keys_limits(),
    meta: {
      label: () => m.api_keys_limits(),
      cellClass: 'font-technical whitespace-normal text-xs leading-5 text-muted-foreground tabular-nums',
    },
    size: 190,
  }),
  apiKeyColumnHelper.accessor((apiKey) => modelAccessLabel(apiKey), {
    id: 'modelAccess',
    header: () => m.api_keys_model_access(),
    cell: (context) => renderSnippet(modelAccessCell, context),
    meta: { label: () => m.api_keys_model_access() },
    size: 220,
  }),
  apiKeyColumnHelper.accessor('expires_at', {
    header: () => m.api_keys_expires(),
    cell: (context) => expirationLabel(context.getValue()),
    meta: { label: () => m.api_keys_expires(), cellClass: 'font-technical whitespace-nowrap text-xs tabular-nums' },
    size: 160,
  }),
  apiKeyColumnHelper.accessor('is_enabled', {
    header: () => m.common_status(),
    cell: (context) => renderSnippet(apiKeyStatusCell, context),
    meta: { label: () => m.common_status() },
    size: 120,
  }),
  apiKeyColumnHelper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(apiKeyActionsCell, context),
    enableHiding: false,
    enableSorting: false,
    meta: { label: () => m.common_actions(), align: 'end', exportable: false },
    size: 64,
  }),
])

function getApiKeyRowId(apiKey: ApiKey): string {
  return apiKey.id
}

function maskedKey(key: string): string {
  return key.length <= 14 ? '••••••••••••' : `${key.slice(0, 6)}••••••••${key.slice(-4)}`
}

function expirationLabel(expiresAt: string | null | undefined): string {
  if (!expiresAt) return m.api_keys_never()
  return Number.isNaN(new Date(expiresAt).valueOf()) ? m.api_keys_invalid_date() : formatLogTime(expiresAt)
}

function limitsLabel(apiKey: ApiKey): string {
  return m.api_keys_concurrent_executions_value({ limit: apiKey.concurrency_limit ?? m.api_key_editor_unlimited() })
}

function modelAccessLabel(apiKey: ApiKey): string {
  return apiKey.model_ids.length === 0
    ? m.api_keys_all_permitted_models()
    : apiKey.model_ids
        .map((id) => {
          const model = models.find((candidate) => candidate.id === id)
          return model ? effectiveModelDisplayName(model) : id
        })
        .join(', ')
}

function openApiKey(apiKey: ApiKey, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  openEditor(apiKey)
}

function handleApiKeyTableRowClick({ event, original }: DataTableRowPointerEvent<ApiKey>): void {
  openApiKey(original, event)
}

function handleApiKeyRowKeydown(event: KeyboardEvent, apiKey: ApiKey): void {
  if (event.key !== 'Enter' || event.target !== event.currentTarget) return
  event.preventDefault()
  openEditor(apiKey)
}

function openCreate(): void {
  openEditor()
}

function openEditor(apiKey?: ApiKey): void {
  editorApiKey = apiKey
  editorSession += 1
  editorOpen = true
}

function askDelete(apiKey: ApiKey): void {
  deleteTarget = apiKey
  deleteOpen = true
}

async function toggleKey(apiKey: ApiKey): Promise<void> {
  actingKeyId = apiKey.id
  try {
    await admin.apiKeys.update(apiKey.id, { is_enabled: !apiKey.is_enabled })
    await queryClient.invalidateQueries({ queryKey: ['api-keys'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingKeyId = undefined
  }
}

async function deleteKey(): Promise<void> {
  if (!deleteTarget) return
  actingKeyId = deleteTarget.id
  try {
    await admin.apiKeys.delete(deleteTarget.id)
    await queryClient.invalidateQueries({ queryKey: ['api-keys'] })
    deleteOpen = false
    deleteTarget = undefined
    toast.success(m.api_keys_api_key_deleted())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingKeyId = undefined
  }
}
</script>

<svelte:head><title>{m.api_keys_api_keys()} · Stravia</title></svelte:head>

{#snippet createKeyAction()}
  <Button onclick={openCreate}><PlusIcon data-icon="inline-start" />{m.common_create_api_key()}</Button>
{/snippet}

{#snippet keyActions(apiKey: ApiKey)}
  <DropdownMenu.Root>
    <DropdownMenu.Trigger>
      {#snippet child({ props })}
        <Button
          {...props}
          size="icon-sm"
          variant="ghost"
          aria-label={m.api_keys_more_actions_value({ name: apiKey.name })}><MoreHorizontalIcon /></Button>
      {/snippet}
    </DropdownMenu.Trigger>
    <DropdownMenu.Content class="w-48" align="end">
      <DropdownMenu.Group
        ><DropdownMenu.Item onSelect={() => void toggleKey(apiKey)} disabled={actingKeyId === apiKey.id}
          >{apiKey.is_enabled ? m.api_keys_disable_api_key() : m.api_keys_enable_api_key()}</DropdownMenu.Item
        ></DropdownMenu.Group>
      <DropdownMenu.Separator />
      <DropdownMenu.Group
        ><DropdownMenu.Item variant="destructive" onSelect={() => askDelete(apiKey)}
          >{m.api_keys_delete_api_key()}</DropdownMenu.Item
        ></DropdownMenu.Group>
    </DropdownMenu.Content>
  </DropdownMenu.Root>
{/snippet}

{#snippet maskedKeyCell(context: DataTableCellContext<ApiKey>)}
  <span class="font-technical text-xs" aria-label={m.api_keys_masked_api_key()}>
    {maskedKey(context.row.original.key)}
  </span>
{/snippet}

{#snippet modelAccessCell(context: DataTableCellContext<ApiKey>)}
  {@const value = modelAccessLabel(context.row.original)}
  <p class="truncate" title={value}>{value}</p>
{/snippet}

{#snippet apiKeyStatusCell(context: DataTableCellContext<ApiKey>)}
  {@const apiKey = context.row.original}
  <StatusIndicator
    compact
    label={apiKey.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
    tone={apiKey.is_enabled ? 'healthy' : 'neutral'} />
{/snippet}

{#snippet apiKeyActionsCell(context: DataTableCellContext<ApiKey>)}
  <div class="flex justify-end gap-1">{@render keyActions(context.row.original)}</div>
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.common_setup()}
    title={m.api_keys_api_keys()}
    description={m.api_keys_page_summary()}
    actions={apiKeys.length > 0 ? createKeyAction : undefined} />

  <section class="route-section" aria-labelledby="api-key-table-title">
    <div class="route-section-header">
      <div>
        <h2 id="api-key-table-title" class="route-section-title">{m.api_keys_client_credentials()}</h2>
        <p class="route-section-description">
          {m.api_keys_secrets_stay_masked_here_shown_only_once_creation()}
        </p>
      </div>
    </div>

    {#if apiKeysQuery.isPending || modelsQuery.isPending}
      <div class="flex flex-col border-y" aria-label={m.api_keys_loading_api_keys()}>
        {#each Array(5) as _, index (index)}<div
            class="grid grid-cols-[2fr_2fr_3fr_1fr] gap-4 border-b p-3 last:border-b-0">
            <Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" />
          </div>{/each}
      </div>
    {:else if apiKeysQuery.isError || modelsQuery.isError}
      <div class="border-y py-6">
        <p class="text-sm font-medium text-destructive">
          {m.api_keys_api_keys_not_loaded()}
        </p>
        <p class="mt-1 text-sm text-muted-foreground">
          {localizeBackendErrorMessage(apiKeysQuery.error ?? modelsQuery.error)}
        </p>
        <Button
          class="mt-3"
          variant="outline"
          onclick={() => void Promise.all([apiKeysQuery.refetch(), modelsQuery.refetch()])}>{m.common_retry()}</Button>
      </div>
    {:else if apiKeys.length === 0}
      <Empty.Root class="border-y py-10"
        ><Empty.Header
          ><Empty.Title>{m.api_keys_no_api_keys_created()}</Empty.Title><Empty.Description
            >{m.api_keys_secret_copy_notice()}</Empty.Description
          ></Empty.Header
        ><Empty.Content><Button onclick={openCreate}>{m.api_keys_create_first_api_key()}</Button></Empty.Content
        ></Empty.Root>
    {:else}
      <div class="route-desktop-table">
        <DataTable
          data={apiKeys}
          columns={apiKeyColumns}
          labels={tableLabels}
          getRowId={getApiKeyRowId}
          ariaLabel={m.api_keys_client_credentials()}
          stripedRows
          sortMode="multiple"
          resizableColumns
          onRowClick={handleApiKeyTableRowClick} />
      </div>
      <div class="route-mobile-list">
        {#each apiKeys as apiKey (apiKey.id)}
          <div
            class="route-mobile-row cursor-pointer"
            role="link"
            tabindex="0"
            onclick={(event) => openApiKey(apiKey, event)}
            onkeydown={(event) => handleApiKeyRowKeydown(event, apiKey)}>
            <div class="min-w-0">
              <p class="truncate font-medium">{apiKey.name}</p>
              <p class="font-technical mt-1 text-xs text-muted-foreground">{maskedKey(apiKey.key)}</p>
              <p class="font-technical mt-2 text-xs text-muted-foreground tabular-nums">{limitsLabel(apiKey)}</p>
              <p class="mt-1 truncate text-xs text-muted-foreground">{modelAccessLabel(apiKey)}</p>
              <div class="mt-1 flex flex-wrap items-center gap-x-3">
                <StatusIndicator
                  compact
                  label={apiKey.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
                  tone={apiKey.is_enabled ? 'healthy' : 'neutral'} /><span
                  class="font-technical text-xs text-muted-foreground">{expirationLabel(apiKey.expires_at)}</span>
              </div>
            </div>
            <div class="flex items-start gap-1">{@render keyActions(apiKey)}</div>
          </div>
        {/each}
      </div>
    {/if}
  </section>
</div>

{#key editorSession}
  <ApiKeyEditor
    bind:open={editorOpen}
    apiKey={editorApiKey}
    {models}
    webSearchEnabled={webSearchQuery.data?.enabled ?? false}
    mediaUnderstandingEnabled={mediaUnderstandingQuery.data?.enabled ?? false} />
{/key}

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content
    ><AlertDialog.Header
      ><AlertDialog.Title
        >{deleteTarget
          ? m.api_keys_delete_named_key({ name: deleteTarget.name })
          : m.api_keys_delete_api_key_question()}</AlertDialog.Title
      ><AlertDialog.Description
        >{deleteTarget
          ? m.api_keys_delete_value_clients_using_lose_access_immediately({ name: deleteTarget.name })
          : ''}</AlertDialog.Description
      ></AlertDialog.Header
    ><AlertDialog.Footer
      ><AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel><AlertDialog.Action
        variant="destructive"
        onclick={() => void deleteKey()}>{m.api_keys_delete_api_key_label()}</AlertDialog.Action
      ></AlertDialog.Footer
    ></AlertDialog.Content>
</AlertDialog.Root>
