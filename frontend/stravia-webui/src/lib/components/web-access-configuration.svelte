<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'
import Globe2Icon from '@lucide/svelte/icons/globe-2'
import PlusIcon from '@lucide/svelte/icons/plus'
import { toast } from 'svelte-sonner'

import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { formatDuration } from '#lib/format.js'
import type { WebAccessSettings, WebProvider, WebProviderKind } from '#lib/types.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import SecretInput from '#lib/components/secret-input.svelte'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Checkbox } from '#lib/components/ui/checkbox/index.js'
import { Button, buttonVariants } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import FieldHelp from '#lib/components/ui/field/field-help.svelte'
import { Switch } from '#lib/components/ui/switch/index.js'

const queryClient = useQueryClient()
const kindsQuery = createQuery(() => ({ queryKey: ['web-provider-kinds'], queryFn: admin.webAccess.kinds.list }))
const providersQuery = createQuery(() => ({ queryKey: ['web-providers'], queryFn: admin.webAccess.providers.list }))
const settingsQuery = createQuery(() => ({ queryKey: ['web-access-settings'], queryFn: admin.webAccess.settings.get }))

let editorOpen = $state(false)
let editingProvider = $state<WebProvider>()
let editorName = $state('')
let editorKind = $state<WebProviderKind>('')
let editorSecret = $state('')
let editorUseProxy = $state(false)
let savingEditor = $state(false)
let editorError = $state('')
let actingProviderId = $state<string>()
let savingSettings = $state(false)
let settingsError = $state('')
let deleteTarget = $state<WebProvider>()
let deleteOpen = $state(false)

const kinds = $derived(kindsQuery.data ?? [])
const kindDescriptorMap = $derived(new Map(kinds.map((descriptor) => [descriptor.kind, descriptor])))
const webProviders = $derived(providersQuery.data ?? [])
const settings = $derived<WebAccessSettings>(settingsQuery.data ?? { search_provider_ids: [], fetch_provider_ids: [] })
const settingsUnavailable = $derived(settingsQuery.isPending || settingsQuery.isError)
const creatableKinds = $derived(
  kinds.filter(
    (descriptor) => !descriptor.singleton || !webProviders.some((provider) => provider.kind === descriptor.kind),
  ),
)
const canCreate = $derived(kindsQuery.isSuccess && providersQuery.isSuccess && creatableKinds.length > 0)
const editorDescriptor = $derived(kindDescriptorMap.get(editorKind))
const editorRequiresApiKey = $derived(
  editorDescriptor?.requires_api_key ?? Boolean(editingProvider?.api_key_configured),
)

function formatKindLabel(kind: WebProviderKind): string {
  const trimmed = kind.trim()
  if (!trimmed) return kind
  return trimmed
    .split(/[-_\s]+/)
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ')
}

// 草稿离开保护：打开编辑器时的字段是基线；保存中锁定关闭。
const editorBaseline = new DraftBaseline(() => ({ editorName, editorKind, editorSecret, editorUseProxy }))
const editorGuard = new DraftGuard({ dirty: () => editorOpen && editorBaseline.dirty, busy: () => savingEditor })

function openCreate(): void {
  const firstKind = creatableKinds[0]?.kind
  if (!firstKind) return
  editingProvider = undefined
  editorName = ''
  editorKind = firstKind
  editorSecret = ''
  editorUseProxy = false
  editorError = ''
  editorBaseline.reset()
  editorOpen = true
}

function openEdit(provider: WebProvider): void {
  if (!provider.registered) return
  editingProvider = provider
  editorName = provider.name
  editorKind = provider.kind
  editorSecret = ''
  editorUseProxy = provider.use_proxy
  editorError = ''
  editorBaseline.reset()
  editorOpen = true
}

async function refreshWebAccess(): Promise<void> {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ['web-provider-kinds'] }),
    queryClient.invalidateQueries({ queryKey: ['web-providers'] }),
    queryClient.invalidateQueries({ queryKey: ['web-access-settings'] }),
  ])
}

async function saveEditor(): Promise<void> {
  if (savingEditor) return
  if (!editorName.trim()) {
    editorError = m.web_access_configuration_service_name_required()
    toast.error(editorError)
    return
  }
  if (!editingProvider && !editorKind) {
    editorError = m.web_access_configuration_service_kind_required()
    toast.error(editorError)
    return
  }
  if (editorRequiresApiKey && !editorSecret.trim() && (!editingProvider || !editingProvider.api_key_configured)) {
    editorError = m.web_access_configuration_api_key_required()
    toast.error(editorError)
    return
  }

  savingEditor = true
  editorError = ''
  try {
    if (editingProvider) {
      await admin.webAccess.providers.update(editingProvider.id, {
        name: editorName.trim(),
        use_proxy: editorUseProxy,
        ...(editorRequiresApiKey && editorSecret.trim() ? { api_key: editorSecret.trim() } : {}),
      })
    } else {
      await admin.webAccess.providers.create({
        name: editorName.trim(),
        kind: editorKind,
        ...(editorRequiresApiKey ? { api_key: editorSecret.trim() } : {}),
        use_proxy: editorUseProxy,
      })
    }
    await refreshWebAccess()
    editorBaseline.reset()
    editorOpen = false
    toast.success(m.web_access_configuration_search_service_saved())
  } catch (error) {
    editorError = localizeBackendErrorMessage(error)
    toast.error(editorError)
  } finally {
    savingEditor = false
  }
}

async function saveSettings(next: WebAccessSettings): Promise<void> {
  if (settingsUnavailable || savingSettings) return
  savingSettings = true
  settingsError = ''
  try {
    const saved = await admin.webAccess.settings.update({
      search_provider_ids: next.search_provider_ids,
      fetch_provider_ids: next.fetch_provider_ids,
    })
    queryClient.setQueryData(['web-access-settings'], saved)
  } catch (error) {
    const code = error && typeof error === 'object' && 'code' in error ? (error as { code?: unknown }).code : undefined
    if (code === 'WEB_PROVIDER_UNAVAILABLE') {
      void queryClient.invalidateQueries({ queryKey: ['web-providers'] })
    }
    settingsError = localizeBackendErrorMessage(error)
    toast.error(settingsError)
  } finally {
    savingSettings = false
  }
}

function toggleCapability(provider: WebProvider, capability: 'search' | 'fetch'): void {
  const key = capability === 'search' ? 'search_provider_ids' : 'fetch_provider_ids'
  const ids = settings[key]
  const enabling = !ids.includes(provider.id)
  if (enabling && (!provider.registered || !provider.available || provider.capabilities?.[capability] !== true)) {
    toast.error(provider.unavailable_reason || m.backend_error_web_provider_unavailable())
    return
  }
  const nextIds = enabling ? [...ids, provider.id] : ids.filter((id) => id !== provider.id)
  void saveSettings({ ...settings, [key]: nextIds })
}

function moveProvider(providerId: string, capability: 'search' | 'fetch', direction: -1 | 1): void {
  const key = capability === 'search' ? 'search_provider_ids' : 'fetch_provider_ids'
  const ids = [...settings[key]]
  const from = ids.indexOf(providerId)
  const to = from + direction
  if (from < 0 || to < 0 || to >= ids.length) return
  ;[ids[from], ids[to]] = [ids[to], ids[from]]
  void saveSettings({ ...settings, [key]: ids })
}

async function testProvider(provider: WebProvider): Promise<void> {
  if (!provider.registered) return
  actingProviderId = provider.id
  try {
    const result = await admin.webAccess.providers.test(provider.id)
    if (result.success) {
      toast.success(m.common_service_response_time({ duration: formatDuration(result.latency_ms) }))
    } else {
      toast.error(result.error || m.common_connection_test_failed())
    }
    void queryClient.invalidateQueries({ queryKey: ['web-providers'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}

async function deleteProvider(): Promise<void> {
  if (!deleteTarget || !deleteTarget.allow_delete || actingProviderId) return
  actingProviderId = deleteTarget.id
  try {
    await admin.webAccess.providers.delete(deleteTarget.id)
    await refreshWebAccess()
    deleteOpen = false
    deleteTarget = undefined
    toast.success(m.web_access_configuration_search_service_deleted())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}
</script>

<!-- 单一设置列内的来源配置：每个已连接服务一行，行内是搜索／网页读取的启用与顺序；不可用或未注册状态就地提示。能力总开关只在页面第一行。 -->
<section id="web-search-sources" class="route-section @container/sources" aria-labelledby="web-providers-title">
  <div class="route-section-header">
    <div class="flex min-w-0 items-center gap-2">
      <h2 id="web-providers-title" class="route-section-title">{m.web_access_configuration_sources_title()}</h2>
      {#if webProviders.length > 0}
        <FieldHelp hint={m.web_access_configuration_enabled_services_tried_top_bottom()} />
      {/if}
    </div>
    {#if canCreate}
      <Button size="sm" onclick={openCreate}>
        <PlusIcon data-icon="inline-start" />{m.common_connect_service()}
      </Button>
    {/if}
  </div>

  <div class="flex flex-col gap-3">
    {#if settingsQuery.isPending || savingSettings}
      <p class="flex items-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner aria-hidden="true" />{savingSettings
          ? m.web_access_configuration_saving_sources()
          : m.common_settings_loading()}
      </p>
    {/if}
    {#if settingsError}
      <Alert.Root variant="destructive"><Alert.Description>{settingsError}</Alert.Description></Alert.Root>
    {/if}
    {#if settingsQuery.isError}
      <RequestFailure
        title={m.web_access_configuration_web_search_settings_not_loaded()}
        message={localizeBackendErrorMessage(settingsQuery.error)}
        retry={() => settingsQuery.refetch()}
        retrying={settingsQuery.isFetching} />
    {/if}
    {#if kindsQuery.isError}
      <RequestFailure
        title={m.web_access_configuration_provider_kinds_not_loaded()}
        message={localizeBackendErrorMessage(kindsQuery.error)}
        retry={() => kindsQuery.refetch()}
        retrying={kindsQuery.isFetching} />
    {/if}
    {#if providersQuery.isError && providersQuery.data !== undefined}
      <RequestFailure
        message={localizeBackendErrorMessage(providersQuery.error)}
        retry={() => providersQuery.refetch()}
        retrying={providersQuery.isFetching} />
    {/if}
    {#if providersQuery.isPending || (webProviders.length === 0 && kindsQuery.isPending)}
      <div class="flex items-center gap-2 border-y py-8" role="status">
        <Spinner aria-hidden="true" />{m.web_access_configuration_loading_search_services()}
      </div>
    {:else if providersQuery.isError && providersQuery.data === undefined}
      <RequestFailure
        title={m.web_access_configuration_search_services_not_loaded()}
        message={localizeBackendErrorMessage(providersQuery.error)}
        retry={() => providersQuery.refetch()}
        retrying={providersQuery.isFetching} />
    {:else if webProviders.length === 0 && kindsQuery.isError}
      <!-- 可用类型加载失败已由上方 RequestFailure 呈现，避免误判为未安装扩展。 -->
    {:else if webProviders.length === 0 && kinds.length === 0}
      <Empty.Root class="border-y py-8">
        <Empty.Header>
          <Empty.Media variant="icon"><Globe2Icon /></Empty.Media>
          <Empty.Title>{m.web_access_configuration_no_provider_kinds_title()}</Empty.Title>
          <Empty.Description>{m.web_access_configuration_no_provider_kinds_description()}</Empty.Description>
        </Empty.Header>
      </Empty.Root>
    {:else if webProviders.length === 0}
      <Empty.Root class="border-y py-8">
        <Empty.Header>
          <Empty.Media variant="icon"><Globe2Icon /></Empty.Media>
          <Empty.Title>{m.web_access_configuration_no_search_services_connected()}</Empty.Title>
          <Empty.Description>{m.web_access_configuration_enable_prerequisite()}</Empty.Description>
        </Empty.Header>
        {#if canCreate}
          <Empty.Content>
            <Button variant="outline" onclick={openCreate}>{m.common_connect_first_service()}</Button>
          </Empty.Content>
        {/if}
      </Empty.Root>
    {:else}
      <div class="divide-y border-y">
        {#each webProviders as provider (provider.id)}
          <div class="flex flex-col gap-2 py-3">
            <div class="flex flex-wrap items-center gap-x-4 gap-y-2">
              <div class="min-w-0 flex-1">
                <div class="flex flex-wrap items-center gap-2">
                  <p class="font-medium">{provider.name}</p>
                  {#if formatKindLabel(provider.kind) !== provider.name}
                    <Badge variant="secondary">{formatKindLabel(provider.kind)}</Badge>
                  {/if}
                  {#if !provider.registered}
                    <Badge variant="destructive" class="gap-1.5" role="status">
                      <span class="size-1.5 rotate-45 rounded-[1px] bg-current" aria-hidden="true"></span>
                      {m.web_access_provider_not_installed()}
                    </Badge>
                  {:else if !provider.available}
                    <StatusIndicator compact tone="warning" label={m.web_access_provider_unavailable_status()} />
                  {/if}
                </div>
                {#if !provider.registered}
                  <p class="mt-1 text-sm break-words text-destructive" role="status">
                    {provider.unavailable_reason ||
                      m.web_access_provider_not_installed_description({ kind: provider.kind })}
                  </p>
                {:else if !provider.available}
                  <p class="mt-1 text-sm break-words text-warning" role="status">
                    {provider.unavailable_reason || m.backend_error_web_provider_unavailable()}
                  </p>
                {/if}
                {#if provider.registered && provider.last_test_success === true}
                  <p class="mt-1 text-xs text-muted-foreground">
                    {m.web_access_configuration_last_connection_test_succeeded()}
                  </p>
                {:else if provider.registered && provider.last_test_success === false}
                  <p class="mt-1 text-xs text-muted-foreground">
                    {m.web_access_configuration_last_connection_test_failed()}
                  </p>
                {/if}
              </div>
              <div class="flex flex-wrap gap-2">
                {#if provider.registered}
                  <Button
                    variant="outline"
                    size="sm"
                    class="min-w-20"
                    aria-busy={actingProviderId === provider.id}
                    disabled={actingProviderId === provider.id}
                    onclick={() => void testProvider(provider)}>
                    {#if actingProviderId === provider.id}
                      <Spinner data-icon="inline-start" aria-label={m.web_access_configuration_testing_connection()} />
                      {m.web_access_configuration_testing()}
                    {:else}
                      {m.web_access_configuration_test()}
                    {/if}
                  </Button>
                  <Button variant="ghost" size="sm" onclick={() => openEdit(provider)}>{m.common_edit()}</Button>
                {/if}
                {#if provider.allow_delete}
                  <Button
                    variant="ghost"
                    size="sm"
                    class="text-destructive"
                    onclick={() => {
                      deleteTarget = provider
                      deleteOpen = true
                    }}>
                    {m.web_access_configuration_delete()}
                  </Button>
                {/if}
              </div>
            </div>
            {#if settingsQuery.data}
              <div class="grid gap-x-8 gap-y-1 @min-[32rem]/sources:grid-cols-2">
                {@render capabilityCell(provider, 'search')}
                {@render capabilityCell(provider, 'fetch')}
              </div>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
</section>

{#snippet capabilityCell(provider: WebProvider, capability: 'search' | 'fetch')}
  {@const isSearch = capability === 'search'}
  {@const label = isSearch ? m.web_access_configuration_web_search() : m.web_access_page_access_label()}
  {@const supported = provider.capabilities?.[capability] === true}
  {@const ids = isSearch ? settings.search_provider_ids : settings.fetch_provider_ids}
  {@const enabled = ids.includes(provider.id)}
  {@const orderIndex = ids.indexOf(provider.id)}
  {@const canToggle = enabled || (provider.registered && provider.available && supported)}
  <div class="flex min-h-8 items-center gap-2">
    {#if supported || enabled}
      <Checkbox
        id={`web-access-${capability}-${provider.id}`}
        bind:checked={() => enabled, () => toggleCapability(provider, capability)}
        disabled={settingsUnavailable || savingSettings || !canToggle}
        aria-label={m.web_access_use_provider_for_capability({ provider: provider.name, capability: label })} />
      <span class="min-w-0 flex-1 text-sm text-muted-foreground" aria-hidden="true">{label}</span>
      {#if enabled}
        <span class="w-4 text-center font-technical text-xs text-muted-foreground tabular-nums">{orderIndex + 1}</span>
        <Button
          size="icon-sm"
          variant="ghost"
          disabled={settingsUnavailable || savingSettings || !provider.registered || orderIndex === 0}
          aria-label={m.web_access_configuration_move_value_up({ name: provider.name, capability: label })}
          onclick={() => moveProvider(provider.id, capability, -1)}>
          <ArrowUpIcon />
        </Button>
        <Button
          size="icon-sm"
          variant="ghost"
          disabled={settingsUnavailable || savingSettings || !provider.registered || orderIndex === ids.length - 1}
          aria-label={m.web_access_configuration_move_value_down({ name: provider.name, capability: label })}
          onclick={() => moveProvider(provider.id, capability, 1)}>
          <ArrowDownIcon />
        </Button>
      {/if}
    {:else}
      <span class="min-w-0 flex-1 text-sm text-muted-foreground" aria-hidden="true">{label}</span>
      <span class="text-sm text-muted-foreground" aria-hidden="true">—</span>
      <span class="sr-only">{label}: {m.web_access_capability_unsupported()}</span>
    {/if}
  </div>
{/snippet}

<Sheet.Root bind:open={() => editorOpen, editorGuard.openChange((next) => (editorOpen = next))}>
  <Sheet.Content
    side="right"
    class="route-overlay-content w-full! gap-0 overflow-hidden p-0"
    closeLabel={m.common_close()}
    locked={editorGuard.busy}>
    <Sheet.Header class="border-b pr-12">
      <Sheet.Title>
        {editingProvider
          ? m.web_access_configuration_edit_search_service()
          : m.web_access_configuration_connect_search_service()}
      </Sheet.Title>
    </Sheet.Header>
    <form
      class="route-overlay-form"
      onsubmit={(event) => {
        event.preventDefault()
        void saveEditor()
      }}>
      <div class="route-overlay-body">
        <Field.Group>
          <Field.Field>
            <Field.Label for="web-provider-name">{m.common_name()}</Field.Label>
            <Input id="web-provider-name" bind:value={editorName} disabled={savingEditor} required />
          </Field.Field>
          <Field.Field>
            <Field.Label for="web-provider-kind" hint={m.web_access_configuration_account_help()}>
              {m.web_access_configuration_service()}
            </Field.Label>
            <Select.Root
              type="single"
              value={editorKind}
              disabled={savingEditor || Boolean(editingProvider)}
              onValueChange={(next: string) => {
                editorKind = next
                editorSecret = ''
                editorError = ''
              }}>
              <Select.Trigger id="web-provider-kind" class="w-full">
                {editorKind ? formatKindLabel(editorKind) : ''}
              </Select.Trigger>
              <Select.Content>
                <Select.Group>
                  {#each editingProvider ? kinds : creatableKinds as descriptor (descriptor.kind)}
                    {@const label = formatKindLabel(descriptor.kind)}
                    <Select.Item value={descriptor.kind} {label}>{label}</Select.Item>
                  {/each}
                </Select.Group>
              </Select.Content>
            </Select.Root>
          </Field.Field>
          <Field.Field orientation="horizontal">
            <Field.Content>
              <Field.Label for="web-provider-use-proxy">{m.common_use_proxy()}</Field.Label>
              <Field.Description>{m.common_send_requests_service_proxy_configured_settings()}</Field.Description>
            </Field.Content>
            <Switch
              id="web-provider-use-proxy"
              disabled={savingEditor}
              checked={editorUseProxy}
              onCheckedChange={(checked: boolean) => (editorUseProxy = checked)} />
          </Field.Field>
          {#if editorRequiresApiKey}
            <Field.Field>
              <Field.Label for="web-provider-secret">{m.common_api_key()}</Field.Label>
              <SecretInput
                id="web-provider-secret"
                disabled={savingEditor}
                resetKey={`${editorOpen}:${editingProvider?.id ?? 'new'}:${editorKind}`}
                autocomplete="new-password"
                bind:value={editorSecret}
                placeholder={editingProvider?.api_key_configured
                  ? m.web_access_configuration_leave_blank_keep_existing()
                  : ''} />
              {#if editingProvider?.api_key_configured}
                <Field.Description>{m.web_access_configuration_api_key_configured_hint()}</Field.Description>
              {/if}
            </Field.Field>
          {/if}
        </Field.Group>
        {#if editorError}<Alert.Root variant="destructive">
            <Alert.Description>{editorError}</Alert.Description>
          </Alert.Root>{/if}
      </div>
      <Sheet.Footer class="route-overlay-footer pb-[max(0.75rem,env(safe-area-inset-bottom))]">
        <Button
          type="button"
          variant="outline"
          disabled={savingEditor}
          onclick={() => editorGuard.request(() => (editorOpen = false))}>
          {m.common_cancel()}
        </Button>
        <Button type="submit" aria-busy={savingEditor} disabled={savingEditor}>
          {#if savingEditor}<Spinner data-icon="inline-start" />{/if}{m.web_access_configuration_save_service()}
        </Button>
      </Sheet.Footer>
    </form>
  </Sheet.Content>
</Sheet.Root>

<DraftDiscardDialog guard={editorGuard} />

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.web_access_configuration_delete_search_service()}</AlertDialog.Title>
      <AlertDialog.Description>
        {m.web_access_service_no_longer_used({ name: deleteTarget?.name ?? m.common_this_service() })}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action class={buttonVariants({ variant: 'destructive' })} onclick={() => void deleteProvider()}>
        {m.web_access_configuration_delete()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
