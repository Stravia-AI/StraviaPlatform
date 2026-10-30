<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { renderSnippet } from '@tanstack/svelte-table'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import PlusIcon from '@lucide/svelte/icons/plus'
import { toast } from 'svelte-sonner'
import { onMount } from 'svelte'

import { admin, currencyQueryOptions } from '#lib/admin-client.js'
import { getConnectSetup } from '#lib/connect-setup.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { getDataTableLabels } from '#lib/data-table-labels.js'
import { formatLogTime } from '#lib/format.js'
import { displayCurrency, formatCostAmount } from '#lib/money.js'
import type { ApiKey } from '#lib/types.js'
import ApiKeyEditor from '#lib/components/api-key-editor.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import RequestFailure from '#lib/components/request-failure.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import {
  DataTable,
  createDataTableColumnHelper,
  type DataTableCellContext,
  type DataTableRowPointerEvent,
} from '#lib/components/ui/data-table/index.js'
import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { capabilityStatus } from '#lib/capability-status.js'

const queryClient = useQueryClient()
const connectSetup = getConnectSetup()
onMount(() => {
  if (connectSetup.draft && connectSetup.createKey) {
    connectSetup.createKey = false
    openCreate()
  }
})
const apiKeysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const currencyQuery = createQuery(() => currencyQueryOptions())
const display = $derived(displayCurrency(currencyQuery.data ?? null))
const webSearchQuery = createQuery(() => ({ queryKey: ['web-search-config'], queryFn: admin.webSearch.config.get }))
const mediaUnderstandingQuery = createQuery(() => ({
  queryKey: ['media-understanding-config'],
  queryFn: admin.mediaUnderstanding.get,
}))
const mediaGenerationQuery = createQuery(() => ({
  queryKey: ['media-generation-config'],
  queryFn: admin.mediaGeneration.config.get,
}))
const webSearchStatus = $derived(capabilityStatus(webSearchQuery, (config) => config.enabled))
const mediaUnderstandingStatus = $derived(
  capabilityStatus(mediaUnderstandingQuery, (config) => config.image.enabled || config.audio.enabled),
)
const mediaGenerationStatus = $derived(capabilityStatus(mediaGenerationQuery, (config) => config.config.enabled))
let editorOpen = $state(false)
let editorApiKey = $state<ApiKey>()
let editorSession = $state(0)
let deleteTarget = $state<ApiKey>()
let deleteOpen = $state(false)
let actingKeyId = $state<string>()

const apiKeys = $derived(apiKeysQuery.data ?? [])
const models = $derived(modelsQuery.data ?? [])
// 先读取全部查询状态，避免短路跳过订阅后错过先完成的依赖。
const resourcesPending = $derived.by(() => {
  const apiKeys = apiKeysQuery.isPending
  const models = modelsQuery.isPending
  return apiKeys || models
})
const resourcesFetching = $derived.by(() => {
  const apiKeys = apiKeysQuery.isFetching
  const models = modelsQuery.isFetching
  return apiKeys || models
})
const resourceError = $derived.by(() => {
  const apiKeys = apiKeysQuery.error
  const models = modelsQuery.error
  return apiKeys ?? models
})
const resourcesEmpty = $derived(
  !resourcesPending && apiKeysQuery.data !== undefined && modelsQuery.data !== undefined && apiKeys.length === 0,
)
const tableLabels = $derived(getDataTableLabels())
const apiKeyColumnHelper = createDataTableColumnHelper<ApiKey>()
const apiKeyColumns = apiKeyColumnHelper.columns([
  apiKeyColumnHelper.accessor('name', {
    header: () => m.common_name(),
    meta: { label: () => m.common_name(), cellClass: 'font-medium', width: 'fill' },
    size: 160,
  }),
  apiKeyColumnHelper.accessor('key', {
    header: () => m.api_keys_masked_secret(),
    cell: (context) => renderSnippet(maskedKeyCell, context),
    meta: { label: () => m.api_keys_masked_secret(), width: 'compact' },
    size: 170,
  }),
  apiKeyColumnHelper.accessor((apiKey) => limitsLabel(apiKey), {
    id: 'limits',
    header: () => m.api_keys_limits(),
    meta: {
      label: () => m.api_keys_limits(),
      cellClass: 'font-technical whitespace-normal text-xs leading-5 text-muted-foreground tabular-nums',
      width: 'compact',
    },
    size: 190,
  }),
  apiKeyColumnHelper.accessor(
    (apiKey) => (apiKey.spend_available == null ? Infinity : Number(apiKey.spend_available)),
    {
      id: 'availableBalance',
      header: () => m.api_keys_available_balance(),
      cell: (context) => balanceLabel(context.row.original),
      meta: {
        label: () => m.api_keys_available_balance(),
        cellClass: 'font-technical whitespace-nowrap tabular-nums',
        width: 'compact',
      },
      size: 140,
    },
  ),
  apiKeyColumnHelper.accessor('expires_at', {
    header: () => m.api_keys_expires(),
    cell: (context) => expirationLabel(context.getValue()),
    meta: {
      label: () => m.api_keys_expires(),
      cellClass: 'font-technical whitespace-nowrap text-xs tabular-nums',
      width: 'compact',
    },
    size: 160,
  }),
  apiKeyColumnHelper.accessor('is_enabled', {
    header: () => m.common_status(),
    cell: (context) => renderSnippet(apiKeyStatusCell, context),
    meta: { label: () => m.common_status(), width: 'compact' },
    size: 120,
  }),
  apiKeyColumnHelper.display({
    id: 'actions',
    header: () => m.common_actions(),
    cell: (context) => renderSnippet(apiKeyActionsCell, context),
    enableHiding: false,
    enableSorting: false,
    meta: { label: () => m.common_actions(), align: 'end', exportable: false, width: 'compact' },
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
  return m.api_keys_rpm_value({ limit: apiKey.rpm_limit ?? m.api_key_editor_unlimited() })
}

// 可用余额 = 消费上限 − 已消费，由 Core 在展示币种下算好；没有上限的密钥不受余额约束。
function balanceLabel(apiKey: ApiKey): string {
  return apiKey.spend_available == null
    ? m.api_key_editor_unlimited()
    : formatCostAmount(apiKey.spend_available, display.symbol)
}

function openApiKey(apiKey: ApiKey, event: MouseEvent): void {
  if (event.target instanceof Element && event.target.closest('a, button, [role="button"]')) return
  openEditor(apiKey)
}

function handleApiKeyTableRowClick({ event, original }: DataTableRowPointerEvent<ApiKey>): void {
  openApiKey(original, event)
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

<div class={['route-page api-keys-page', { 'route-page-empty': resourcesEmpty }]}>
  <PageHeader title={m.api_keys_api_keys()} actions={apiKeys.length > 0 ? createKeyAction : undefined} />

  <section class="route-section api-keys-workspace" aria-labelledby="api-key-table-title">
    <h2 id="api-key-table-title" class="sr-only">{m.api_keys_client_credentials()}</h2>

    {#if resourceError}
      <RequestFailure
        title={m.api_keys_api_keys_not_loaded()}
        message={localizeBackendErrorMessage(resourceError)}
        retry={() => Promise.all([apiKeysQuery.refetch(), modelsQuery.refetch()])}
        retrying={resourcesFetching} />
    {/if}

    {#if resourcesPending}
      <div class="flex flex-col border-y" aria-label={m.api_keys_loading_api_keys()}>
        {#each Array(5) as _, index (index)}<div
            class="grid grid-cols-[2fr_2fr_3fr_1fr] gap-4 border-b p-3 last:border-b-0">
            <Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" /><Skeleton class="h-6" />
          </div>{/each}
      </div>
    {:else if resourcesEmpty}
      <Empty.Root class="mx-auto my-auto w-full max-w-xl py-10"
        ><Empty.Header><Empty.Title>{m.api_keys_no_api_keys_created()}</Empty.Title></Empty.Header><Empty.Content
          ><Button onclick={openCreate}>{m.api_keys_create_first_api_key()}</Button></Empty.Content
        ></Empty.Root>
    {:else if apiKeysQuery.data !== undefined && modelsQuery.data !== undefined}
      <div class="route-desktop-table min-h-0 flex-1">
        <DataTable
          workbench
          data={apiKeys}
          columns={apiKeyColumns}
          labels={tableLabels}
          getRowId={getApiKeyRowId}
          ariaLabel={m.api_keys_client_credentials()}
          stripedRows
          stickyHeader
          sortMode="multiple"
          resizableColumns
          onRowClick={handleApiKeyTableRowClick} />
      </div>
      <div class="route-mobile-list">
        {#each apiKeys as apiKey (apiKey.id)}
          <div class="route-mobile-row">
            <button type="button" class="min-w-0 cursor-pointer text-start" onclick={() => openEditor(apiKey)}>
              <span class="block truncate font-medium">{apiKey.name}</span>
              <span class="block font-technical mt-1 text-xs text-muted-foreground">{maskedKey(apiKey.key)}</span>
              <span class="block font-technical mt-2 text-xs text-muted-foreground tabular-nums"
                >{limitsLabel(apiKey)}</span>
              <span class="block font-technical mt-1 text-xs text-muted-foreground tabular-nums"
                >{m.api_keys_available_balance()}: {balanceLabel(apiKey)}</span>
              <span class="mt-1 flex flex-wrap items-center gap-x-3">
                <StatusIndicator
                  compact
                  label={apiKey.is_enabled ? m.common_enabled_status() : m.common_disabled_status()}
                  tone={apiKey.is_enabled ? 'healthy' : 'neutral'} /><span
                  class="font-technical text-xs text-muted-foreground">{expirationLabel(apiKey.expires_at)}</span>
              </span>
            </button>
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
    {webSearchStatus}
    {mediaUnderstandingStatus}
    {mediaGenerationStatus} />
{/key}

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>
        {deleteTarget ? m.api_keys_delete_named_key({ name: deleteTarget.name }) : m.api_keys_delete_api_key_question()}
      </AlertDialog.Title>
      <AlertDialog.Description>
        {deleteTarget ? m.api_keys_delete_value_clients_using_lose_access_immediately({ name: deleteTarget.name }) : ''}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" onclick={() => void deleteKey()}
        >{m.api_keys_delete_api_key_label()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
.api-keys-page {
  /* 与壳层一致，标题按内容占位，其余高度留给密钥列表。 */
  height: calc(100svh - 5rem);
}
.api-keys-workspace {
  display: flex;
  flex: 1;
  min-height: 24rem;
  flex-direction: column;
}
@container route-page (max-width: 52rem) {
  .api-keys-workspace {
    flex: none;
    min-height: 0;
  }
}
@media (max-width: 767px) {
  .api-keys-page {
    height: calc(100svh - 4.5rem);
  }
}
</style>
