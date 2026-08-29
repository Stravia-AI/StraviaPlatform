<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import ArrowDownIcon from '@lucide/svelte/icons/arrow-down'
import ArrowUpIcon from '@lucide/svelte/icons/arrow-up'
import Globe2Icon from '@lucide/svelte/icons/globe-2'
import PlusIcon from '@lucide/svelte/icons/plus'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatDuration } from '$lib/format'
import type { WebAccessSettings, WebProvider, WebProviderKind } from '$lib/types'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
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
let savingEditor = $state(false)
let actingProviderId = $state<string>()
let savingSettings = $state(false)
let deleteTarget = $state<WebProvider>()
let deleteOpen = $state(false)

const webProviders = $derived(providersQuery.data ?? [])
const settings = $derived<WebAccessSettings>(
  settingsQuery.data ?? { enabled: false, search_provider_ids: [], fetch_provider_ids: [] },
)
const settingsUnavailable = $derived(settingsQuery.isPending || settingsQuery.isError)

function kindLabel(kind: WebProviderKind): string {
  return ({ exa: 'Exa', brave: 'Brave', tavily: 'Tavily', zhipu: 'Zhipu' } as const)[kind]
}

function supportsFetch(provider: WebProvider): boolean {
  return provider.kind === 'exa' || provider.kind === 'tavily' || provider.kind === 'zhipu'
}

function openCreate(): void {
  editingProvider = undefined
  editorName = ''
  editorKind = 'exa'
  editorSecret = ''
  editorOpen = true
}

function openEdit(provider: WebProvider): void {
  editingProvider = provider
  editorName = provider.name
  editorKind = provider.kind
  editorSecret = ''
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
    toast.error(m.web_access_configuration_service_name_required())
    return
  }
  if (!editingProvider && !editorSecret.trim()) {
    toast.error(m.web_access_configuration_api_key_required())
    return
  }

  savingEditor = true
  try {
    if (editingProvider) {
      await admin.webAccess.providers.update(editingProvider.id, {
        name: editorName.trim(),
        ...(editorSecret.trim() ? { api_key: editorSecret.trim() } : {}),
      })
    } else {
      await admin.webAccess.providers.create({
        name: editorName.trim(),
        kind: editorKind,
        api_key: editorSecret.trim(),
      })
    }
    await refreshWebAccess()
    editorOpen = false
    toast.success(m.web_access_configuration_search_service_saved())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    savingEditor = false
  }
}

async function saveSettings(next: WebAccessSettings): Promise<void> {
  savingSettings = true
  try {
    await admin.webAccess.settings.update(next)
    await queryClient.invalidateQueries({ queryKey: ['web-access-settings'] })
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
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
    <Switch
      checked={settings.enabled}
      disabled={settingsUnavailable || savingSettings}
      aria-label={m.web_access_configuration_enable_web_search_page_access()}
      onCheckedChange={(checked) => void saveSettings({ ...settings, enabled: checked })} />
  </div>
  {#if settingsQuery.isError}
    <div class="border-t pt-4">
      <p class="text-sm font-medium text-destructive">
        {m.web_access_configuration_web_search_settings_not_loaded()}
      </p>
      <Button class="mt-3" variant="outline" onclick={() => void settingsQuery.refetch()}>{m.common_retry()}</Button>
    </div>
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
      <span class="font-technical text-xs text-muted-foreground tabular-nums">{webProviders.length}</span>
      <Button size="sm" onclick={openCreate}>
        <PlusIcon data-icon="inline-start" />{m.common_connect_service()}
      </Button>
    </div>
  </div>

  {#if providersQuery.isPending}
    <div class="border-y py-8 text-sm text-muted-foreground">
      {m.web_access_configuration_loading_search_services()}
    </div>
  {:else if providersQuery.isError}
    <div class="border-y py-6">
      <p class="text-sm font-medium text-destructive">
        {m.web_access_configuration_search_services_not_loaded()}
      </p>
      <Button class="mt-3" variant="outline" onclick={() => void providersQuery.refetch()}>{m.common_retry()}</Button>
    </div>
  {:else if webProviders.length === 0}
    <div class="flex flex-col items-start gap-3 border-y py-8">
      <Globe2Icon class="size-5 text-muted-foreground" />
      <div>
        <p class="font-medium">{m.web_access_configuration_no_search_services_connected()}</p>
        <p class="mt-1 text-sm text-muted-foreground">
          {m.web_access_configuration_enable_prerequisite()}
        </p>
      </div>
      <Button variant="outline" onclick={openCreate}>{m.common_connect_first_service()}</Button>
    </div>
  {:else}
    <div class="divide-y border-y">
      {#each webProviders as provider (provider.id)}
        <div class="grid gap-4 py-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center">
          <div class="min-w-0">
            <div class="flex flex-wrap items-center gap-2">
              <p class="font-medium">{provider.name}</p>
              <Badge variant="secondary">{kindLabel(provider.kind)}</Badge>
              <Badge variant="outline">{m.web_access_configuration_search()}</Badge>
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
            <Button
              variant="ghost"
              size="sm"
              class="text-destructive"
              onclick={() => {
                deleteTarget = provider
                deleteOpen = true
              }}>{m.web_access_configuration_delete()}</Button>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</section>

<div class="grid gap-8 lg:grid-cols-2">
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
              checked={enabled}
              disabled={settingsUnavailable || savingSettings}
              aria-label={m.web_access_use_provider_for_capability({
                provider: provider.name,
                capability: isSearch ? m.web_access_configuration_web_search() : m.web_access_page_access_label(),
              })}
              onCheckedChange={() => toggleCapability(provider, isSearch ? 'search' : 'fetch')} />
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
            <Input id="web-provider-name" bind:value={editorName} required />
          </Field.Field>
          <Field.Field size="select">
            <Field.Label for="web-provider-kind">{m.web_access_configuration_service()}</Field.Label>
            <Select.Root type="single" bind:value={editorKind} disabled={Boolean(editingProvider)}>
              <Select.Trigger id="web-provider-kind" class="w-full">{kindLabel(editorKind)}</Select.Trigger>
              <Select.Content>
                <Select.Item value="exa" label="Exa">Exa</Select.Item>
                <Select.Item value="brave" label="Brave">Brave</Select.Item>
                <Select.Item value="tavily" label="Tavily">Tavily</Select.Item>
                <Select.Item value="zhipu" label="Zhipu">Zhipu</Select.Item>
              </Select.Content>
            </Select.Root>
          </Field.Field>
          <Field.Field size="fill">
            <Field.Label for="web-provider-secret">{m.common_api_key()}</Field.Label>
            <Input
              id="web-provider-secret"
              type="password"
              autocomplete="new-password"
              bind:value={editorSecret}
              placeholder={editingProvider ? m.web_access_configuration_leave_blank_keep_existing() : ''} />
          </Field.Field>
        </Field.Group>
      </div>
      <Sheet.Footer class="route-overlay-footer">
        <Sheet.Close type="button" class={buttonVariants({ variant: 'outline' })}>{m.common_cancel()}</Sheet.Close>
        <Button type="submit" disabled={savingEditor}>
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
