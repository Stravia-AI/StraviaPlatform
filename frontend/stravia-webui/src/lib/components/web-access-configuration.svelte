<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import RequestFailure from '$lib/components/request-failure.svelte'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'
import Globe2Icon from '@lucide/svelte/icons/globe-2'
import PlusIcon from '@lucide/svelte/icons/plus'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatDuration } from '$lib/format'
import type {
  LocalSearchEngineConfigs,
  LocalSearchEngineId,
  WebAccessSettings,
  WebProvider,
  WebProviderKind,
} from '$lib/types'
import * as Alert from '$lib/components/ui/alert'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import * as Empty from '$lib/components/ui/empty'
import SecretInput from '$lib/components/secret-input.svelte'
import { Badge } from '$lib/components/ui/badge'
import { Button, buttonVariants } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

const queryClient = useQueryClient()
const providersQuery = createQuery(() => ({ queryKey: ['web-providers'], queryFn: admin.webAccess.providers.list }))
const settingsQuery = createQuery(() => ({ queryKey: ['web-access-settings'], queryFn: admin.webAccess.settings.get }))

let editorOpen = $state(false)
let editingProvider = $state<WebProvider>()
let editorName = $state('')
let editorKind = $state<WebProviderKind>('exa')
let editorSecret = $state('')
let editorUseProxy = $state(false)
let savingEditor = $state(false)
let editorError = $state('')
let actingProviderId = $state<string>()
let savingSettings = $state(false)
let settingsError = $state('')
let deleteTarget = $state<WebProvider>()
let deleteOpen = $state(false)

const webProviders = $derived(providersQuery.data ?? [])
const settings = $derived<WebAccessSettings>(
  settingsQuery.data ?? { enabled: false, search_provider_ids: [], fetch_provider_ids: [] },
)
const settingsUnavailable = $derived(settingsQuery.isPending || settingsQuery.isError)

function kindLabel(kind: WebProviderKind): string {
  return ({ local: 'Local', exa: 'Exa', zhipu: 'Zhipu' } as const)[kind]
}

const localEngineOptions: ReadonlyArray<{ id: LocalSearchEngineId; label: string }> = [
  { id: 'google', label: 'Google' },
  { id: 'bing', label: 'Bing' },
  { id: 'brave', label: 'Brave' },
  { id: 'baidu', label: 'Baidu' },
  { id: '360', label: '360 Search' },
  { id: 'sogou_weixin', label: 'Sogou Weixin' },
  { id: 'google_scholar', label: 'Google Scholar' },
]

function defaultLocalEngines(): LocalSearchEngineConfigs {
  return Object.fromEntries(
    localEngineOptions.map(({ id }) => [id, { enabled: ['google', 'bing', 'brave', 'baidu'].includes(id) }]),
  ) as LocalSearchEngineConfigs
}

let editorLocalEngines = $state<LocalSearchEngineConfigs>(defaultLocalEngines())

function supportsFetch(provider: WebProvider): boolean {
  return provider.capabilities.fetch
}

function openCreate(): void {
  editingProvider = undefined
  editorName = ''
  editorKind = 'exa'
  editorSecret = ''
  editorUseProxy = false
  editorLocalEngines = defaultLocalEngines()
  editorError = ''
  editorOpen = true
}

function openEdit(provider: WebProvider): void {
  editingProvider = provider
  editorName = provider.name
  editorKind = provider.kind
  editorSecret = ''
  editorUseProxy = provider.use_proxy
  editorLocalEngines = { ...defaultLocalEngines(), ...(provider.local_engines ?? {}) }
  editorError = ''
  editorOpen = true
}

async function refreshWebAccess(): Promise<void> {
  await Promise.all([
    queryClient.invalidateQueries({ queryKey: ['web-providers'] }),
    queryClient.invalidateQueries({ queryKey: ['web-access-settings'] }),
  ])
}

async function saveEditor(): Promise<void> {
  if (!editorName.trim()) {
    editorError = m.web_access_configuration_service_name_required()
    toast.error(editorError)
    return
  }
  if (!editingProvider && editorKind !== 'local' && !editorSecret.trim()) {
    editorError = m.web_access_configuration_api_key_required()
    toast.error(editorError)
    return
  }

  savingEditor = true
  editorError = ''
  try {
    if (editingProvider) {
      await admin.webAccess.providers.update(
        editingProvider.id,
        editorKind === 'local'
          ? { name: editorName.trim(), use_proxy: editorUseProxy, local_engines: editorLocalEngines }
          : {
              name: editorName.trim(),
              use_proxy: editorUseProxy,
              ...(editorSecret.trim() ? { api_key: editorSecret.trim() } : {}),
            },
      )
    } else {
      await admin.webAccess.providers.create({
        name: editorName.trim(),
        kind: editorKind,
        api_key: editorSecret.trim() || undefined,
        use_proxy: editorUseProxy,
      })
    }
    await refreshWebAccess()
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
  savingSettings = true
  settingsError = ''
  try {
    const saved = await admin.webAccess.settings.update(next)
    queryClient.setQueryData(['web-access-settings'], saved)
  } catch (error) {
    settingsError = localizeBackendErrorMessage(error)
    toast.error(settingsError)
  } finally {
    savingSettings = false
  }
}

function toggleCapability(provider: WebProvider, capability: 'search' | 'fetch'): void {
  const key = capability === 'search' ? 'search_provider_ids' : 'fetch_provider_ids'
  const ids = settings[key]
  const nextIds = ids.includes(provider.id) ? ids.filter((id) => id !== provider.id) : [...ids, provider.id]
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
  actingProviderId = provider.id
  try {
    const result = await admin.webAccess.providers.test(provider.id)
    if (result.success) {
      toast.success(m.common_service_response_time({ duration: formatDuration(result.latency_ms) }))
    } else {
      toast.error(result.error || m.common_connection_test_failed())
    }
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    actingProviderId = undefined
  }
}

async function deleteProvider(): Promise<void> {
  if (!deleteTarget) return
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

<section class="route-section" aria-labelledby="web-access-gate-title">
  <div class="route-section-header">
    <div>
      <h2 id="web-access-gate-title" class="route-section-title">
        {m.web_access_configuration_web_search_page_access()}
      </h2>
      <p class="route-section-description">
        {m.web_access_configuration_feature_summary()}
      </p>
    </div>
    {#if settingsQuery.data}
      <div class="flex items-center gap-3" aria-busy={savingSettings}>
        {#if savingSettings}<Spinner aria-hidden="true" />{/if}
        <Switch
          bind:checked={() => settings.enabled, (checked) => void saveSettings({ ...settings, enabled: checked })}
          disabled={settingsUnavailable || savingSettings}
          aria-label={m.web_access_configuration_enable_web_search_page_access()}
          aria-describedby="web-access-save-behavior" />
      </div>
    {/if}
  </div>
  <p id="web-access-save-behavior" class="mb-3 text-sm text-muted-foreground">
    {m.web_access_configuration_immediate()}
  </p>
  {#if settingsQuery.isPending}<p class="text-sm text-muted-foreground" role="status">
      {m.common_settings_loading()}
    </p>{/if}
  {#if settingsError}<Alert.Root variant="destructive"
      ><Alert.Description>{settingsError}</Alert.Description></Alert.Root
    >{/if}
  {#if settingsQuery.isError}
    <RequestFailure
      title={m.web_access_configuration_web_search_settings_not_loaded()}
      message={localizeBackendErrorMessage(settingsQuery.error)}
      retry={() => settingsQuery.refetch()}
      retrying={settingsQuery.isFetching} />
  {/if}
</section>

<section class="route-section" aria-labelledby="web-providers-title">
  <div class="route-section-header">
    <div>
      <h2 id="web-providers-title" class="route-section-title">{m.common_search_services()}</h2>
      <p class="route-section-description">
        {m.web_access_configuration_service_selection_help()}
      </p>
    </div>
    <div class="flex items-center gap-3">
      {#if providersQuery.data}<span class="font-technical text-xs text-muted-foreground tabular-nums"
          >{webProviders.length}</span
        >{/if}
      <Button size="sm" onclick={openCreate}>
        <PlusIcon data-icon="inline-start" />{m.common_connect_service()}
      </Button>
    </div>
  </div>

  {#if providersQuery.isError && providersQuery.data !== undefined}
    <RequestFailure
      message={localizeBackendErrorMessage(providersQuery.error)}
      retry={() => providersQuery.refetch()}
      retrying={providersQuery.isFetching} />
  {/if}
  {#if providersQuery.isPending}
    <div class="flex items-center gap-2 border-y py-8" role="status">
      <Spinner aria-hidden="true" />{m.web_access_configuration_loading_search_services()}
    </div>
  {:else if providersQuery.isError && providersQuery.data === undefined}
    <RequestFailure
      title={m.web_access_configuration_search_services_not_loaded()}
      message={localizeBackendErrorMessage(providersQuery.error)}
      retry={() => providersQuery.refetch()}
      retrying={providersQuery.isFetching} />
  {:else if webProviders.length === 0}
    <Empty.Root class="border-y py-8"
      ><Empty.Header
        ><Empty.Media variant="icon"><Globe2Icon /></Empty.Media><Empty.Title
          >{m.web_access_configuration_no_search_services_connected()}</Empty.Title
        ><Empty.Description>{m.web_access_configuration_enable_prerequisite()}</Empty.Description></Empty.Header
      ><Empty.Content
        ><Button variant="outline" onclick={openCreate}>{m.common_connect_first_service()}</Button></Empty.Content
      ></Empty.Root>
  {:else}
    <div class="divide-y border-y">
      {#each webProviders as provider (provider.id)}
        <div class="grid gap-4 py-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center">
          <div class="min-w-0">
            <div class="flex flex-wrap items-center gap-2">
              <p class="font-medium">{provider.name}</p>
              <Badge variant="secondary">{kindLabel(provider.kind)}</Badge>
              {#if supportsFetch(provider)}<Badge variant="outline">{m.web_access_read_pages_label()}</Badge>{/if}
            </div>
            <p class="mt-1 text-sm text-muted-foreground">
              {#if provider.last_test_success === true}
                {m.web_access_configuration_last_connection_test_succeeded()}
              {:else if provider.last_test_success === false}
                {m.web_access_configuration_last_connection_test_failed()}
              {:else}
                {m.web_access_configuration_connection_not_tested()}
              {/if}
            </p>
          </div>
          <div class="flex flex-wrap gap-2 sm:justify-end">
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
            {#if provider.kind !== 'local'}
              <Button
                variant="ghost"
                size="sm"
                class="text-destructive"
                onclick={() => {
                  deleteTarget = provider
                  deleteOpen = true
                }}>{m.web_access_configuration_delete()}</Button>
            {/if}
          </div>
        </div>
      {/each}
    </div>
  {/if}
</section>

{#if settingsQuery.data && providersQuery.data}
  <div class="grid gap-6 lg:grid-cols-2">
    {#each ['search', 'fetch'] as capability (capability)}
      {@const isSearch = capability === 'search'}
      {@const ids = isSearch ? settings.search_provider_ids : settings.fetch_provider_ids}
      {@const candidates = webProviders.filter((provider) => isSearch || supportsFetch(provider))}
      <section class="route-section" aria-labelledby={`${capability}-priority-title`}>
        <div class="route-section-header">
          <div>
            <h2 id={`${capability}-priority-title`} class="route-section-title">
              {isSearch ? m.web_access_configuration_web_search() : m.web_access_page_access_label()}
            </h2>
            <p class="route-section-description">
              {m.web_access_configuration_enabled_services_tried_top_bottom()}
            </p>
          </div>
        </div>
        <div class="divide-y border-y">
          {#each candidates as provider (provider.id)}
            {@const enabled = ids.includes(provider.id)}
            {@const orderIndex = ids.indexOf(provider.id)}
            <div class="flex min-h-14 items-center gap-3 py-2">
              <Switch
                bind:checked={() => enabled, () => toggleCapability(provider, isSearch ? 'search' : 'fetch')}
                disabled={settingsUnavailable || savingSettings}
                aria-label={m.web_access_use_provider_for_capability({
                  provider: provider.name,
                  capability: isSearch ? m.web_access_configuration_web_search() : m.web_access_page_access_label(),
                })} />
              <span class="min-w-0 flex-1 truncate text-sm font-medium">{provider.name}</span>
              {#if enabled}
                <span class="font-technical text-xs text-muted-foreground tabular-nums">{orderIndex + 1}</span>
                <Button
                  size="icon-sm"
                  variant="ghost"
                  disabled={settingsUnavailable || savingSettings || orderIndex === 0}
                  aria-label={m.web_access_configuration_move_value_up({ name: provider.name })}
                  onclick={() => moveProvider(provider.id, isSearch ? 'search' : 'fetch', -1)}><ArrowUpIcon /></Button>
                <Button
                  size="icon-sm"
                  variant="ghost"
                  disabled={settingsUnavailable || savingSettings || orderIndex === ids.length - 1}
                  aria-label={m.web_access_configuration_move_value_down({ name: provider.name })}
                  onclick={() => moveProvider(provider.id, isSearch ? 'search' : 'fetch', 1)}><ArrowDownIcon /></Button>
              {/if}
            </div>
          {/each}
          {#if candidates.length === 0}
            <p class="py-5 text-sm text-muted-foreground">
              {m.web_access_configuration_no_compatible_services()}
            </p>
          {/if}
        </div>
      </section>
    {/each}
  </div>
{/if}

<Sheet.Root bind:open={editorOpen}>
  <Sheet.Content side="right" class="route-overlay-content w-full! gap-0 overflow-hidden p-0">
    <Sheet.Header class="border-b">
      <Sheet.Title
        >{editingProvider
          ? m.web_access_configuration_edit_search_service()
          : m.web_access_configuration_connect_search_service()}</Sheet.Title>
      <Sheet.Description>{m.web_access_configuration_account_help()}</Sheet.Description>
    </Sheet.Header>
    <form
      class="route-overlay-form"
      onsubmit={(event) => {
        event.preventDefault()
        void saveEditor()
      }}>
      <div class="route-overlay-body">
        <Field.Group>
          <Field.Field size="name">
            <Field.Label for="web-provider-name">{m.common_name()}</Field.Label>
            <Input id="web-provider-name" bind:value={editorName} disabled={savingEditor} required />
          </Field.Field>
          <Field.Field size="select">
            <Field.Label for="web-provider-kind">{m.web_access_configuration_service()}</Field.Label>
            <Select.Root type="single" bind:value={editorKind} disabled={savingEditor || Boolean(editingProvider)}>
              <Select.Trigger id="web-provider-kind" class="w-full">{kindLabel(editorKind)}</Select.Trigger>
              <Select.Content
                ><Select.Group>
                  <Select.Item value="exa" label="Exa">Exa</Select.Item>
                  <Select.Item value="zhipu" label="Zhipu">Zhipu</Select.Item>
                </Select.Group></Select.Content>
            </Select.Root>
          </Field.Field>
          <Field.Field orientation="horizontal">
            <div>
              <Field.Label
                for="web-provider-use-proxy"
                hint={m.common_send_requests_service_proxy_configured_settings()}>{m.common_use_proxy()}</Field.Label>
            </div>
            <Switch
              id="web-provider-use-proxy"
              disabled={savingEditor}
              checked={editorUseProxy}
              onCheckedChange={(checked) => (editorUseProxy = checked)} />
          </Field.Field>
          {#if editorKind === 'local'}
            <Field.Set>
              <Field.Legend>{m.web_access_configuration_local_search_engines()}</Field.Legend>
              <div class="divide-y rounded-md border">
                {#each localEngineOptions as engine (engine.id)}
                  <div class="flex min-h-12 items-center justify-between gap-4 px-3">
                    <span class="text-sm font-medium">{engine.label}</span>
                    <Switch
                      disabled={savingEditor}
                      checked={editorLocalEngines[engine.id].enabled}
                      aria-label={engine.label}
                      onCheckedChange={(checked) => (editorLocalEngines[engine.id].enabled = checked)} />
                  </div>
                {/each}
              </div>
            </Field.Set>
          {:else}
            <Field.Field size="fill">
              <Field.Label for="web-provider-secret">{m.common_api_key()}</Field.Label>
              <SecretInput
                id="web-provider-secret"
                disabled={savingEditor}
                resetKey={`${editorOpen}:${editingProvider?.id ?? 'new'}:${editorKind}`}
                autocomplete="new-password"
                bind:value={editorSecret}
                placeholder={editingProvider ? m.web_access_configuration_leave_blank_keep_existing() : ''} />
            </Field.Field>
          {/if}
        </Field.Group>
        {#if editorError}<Alert.Root variant="destructive"
            ><Alert.Description>{editorError}</Alert.Description></Alert.Root
          >{/if}
      </div>
      <Sheet.Footer class="route-overlay-footer">
        <Sheet.Close type="button" class={buttonVariants({ variant: 'outline' })}>{m.common_cancel()}</Sheet.Close>
        <Button type="submit" disabled={savingEditor} aria-busy={savingEditor}>
          {#if savingEditor}<Spinner data-icon="inline-start" />{/if}{m.web_access_configuration_save_service()}
        </Button>
      </Sheet.Footer>
    </form>
  </Sheet.Content>
</Sheet.Root>

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
