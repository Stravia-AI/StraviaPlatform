<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import LocateFixedIcon from '@lucide/svelte/icons/locate-fixed'
import MoreHorizontalIcon from '@lucide/svelte/icons/more-horizontal'
import PlusIcon from '@lucide/svelte/icons/plus'
import TriangleAlertIcon from '@lucide/svelte/icons/triangle-alert'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { onDestroy } from 'svelte'
import { toast } from 'svelte-sonner'

import {
  admin,
  PROXY_MODE_SYSTEM,
  type OutboundProxyEntry,
  type ProxyIpProbeResult,
  type ProxySettings,
} from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import CountryFlag from '#lib/components/country-flag.svelte'
import { proxyDisplayName, redactProxyUrl } from '#lib/proxy-display.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import SettingsSection from '#lib/components/settings/settings-section.svelte'
import SettingsSheet from '#lib/components/settings/settings-sheet.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import * as AlertDialog from '#lib/components/ui/alert-dialog/index.js'
import { Badge } from '#lib/components/ui/badge/index.js'
import { Button, buttonVariants } from '#lib/components/ui/button/index.js'
import * as DropdownMenu from '#lib/components/ui/dropdown-menu/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'
import { createUuid } from '#lib/uuid.js'

interface ProbeState {
  loading: boolean
  result?: ProxyIpProbeResult
  error?: string
}

const queryClient = useQueryClient()
const proxyQuery = createQuery(() => ({ queryKey: ['outbound-proxy-settings'], queryFn: admin.settings.proxy }))
const systemProxyQuery = createQuery(() => ({
  queryKey: ['outbound-proxy-system'],
  queryFn: admin.settings.systemProxy,
}))

let editorOpen = $state(false)
let editingId = $state<string | null>(null)
let nameDraft = $state('')
let urlDraft = $state('')
let saving = $state(false)
let editorError = $state('')

let deleteTarget = $state<OutboundProxyEntry | null>(null)
let deleteOpen = $state(false)
let deletingId = $state<string | null>(null)

// IP probe results are kept in component memory only and never persisted.
let probes = $state<Record<string, ProbeState>>({})
const probeGenerations: Record<string, number> = {}
let disposed = false

onDestroy(() => {
  disposed = true
})

const proxies = $derived(proxyQuery.data?.proxies ?? [])
const systemProxy = $derived(systemProxyQuery.data)
const systemDetectedUrl = $derived(systemProxy?.detected_url ? redactProxyUrl(systemProxy.detected_url) : null)
const systemSourceLabel = $derived(
  systemProxy?.source === 'env' ? m.settings_proxy_system_source_env() : m.settings_proxy_system_source_os(),
)
const systemProbe = $derived(probes[PROXY_MODE_SYSTEM])

async function saveProxyList(nextProxies: OutboundProxyEntry[]): Promise<ProxySettings> {
  const settings: ProxySettings = { proxies: nextProxies }
  await admin.settings.saveProxy(settings)
  queryClient.setQueryData(['outbound-proxy-settings'], settings)
  return settings
}

function clearProbe(key: string): void {
  probeGenerations[key] = (probeGenerations[key] ?? 0) + 1
  if (!(key in probes)) return
  const next = { ...probes }
  delete next[key]
  probes = next
}

async function probeTarget(proxyMode: string): Promise<void> {
  const generation = (probeGenerations[proxyMode] ?? 0) + 1
  probeGenerations[proxyMode] = generation
  probes = { ...probes, [proxyMode]: { loading: true } }
  try {
    const result = await admin.settings.probeProxy({ proxy_mode: proxyMode })
    if (disposed || probeGenerations[proxyMode] !== generation) return
    probes = { ...probes, [proxyMode]: { loading: false, result } }
  } catch (cause) {
    if (disposed || probeGenerations[proxyMode] !== generation) return
    probes = { ...probes, [proxyMode]: { loading: false, error: localizeBackendErrorMessage(cause) } }
  }
}

function openAdd(): void {
  editingId = null
  nameDraft = ''
  urlDraft = ''
  editorError = ''
  editorOpen = true
}

function openEdit(entry: OutboundProxyEntry): void {
  editingId = entry.id
  nameDraft = entry.name
  urlDraft = entry.url
  editorError = ''
  editorOpen = true
}

async function submitEditor(): Promise<void> {
  if (saving || !proxyQuery.data) return
  const trimmedUrl = urlDraft.trim()
  const trimmedName = nameDraft.trim()
  if (!trimmedUrl) return

  saving = true
  editorError = ''
  try {
    if (editingId === null) {
      const id = createUuid()
      const nextProxies: OutboundProxyEntry[] = [...proxies, { id, name: trimmedName, url: trimmedUrl }]
      await saveProxyList(nextProxies)
      editorOpen = false
      toast.success(m.settings_proxy_added())
      void probeTarget(id)
    } else {
      const targetId = editingId
      const previous = proxies.find((item) => item.id === targetId)
      const urlChanged = previous ? previous.url.trim() !== trimmedUrl : true
      const nextProxies = proxies.map((item) =>
        item.id === targetId ? { ...item, name: trimmedName, url: trimmedUrl } : item,
      )
      await saveProxyList(nextProxies)
      if (urlChanged) clearProbe(targetId)
      editorOpen = false
      toast.success(m.settings_proxy_settings_saved())
    }
  } catch (cause) {
    editorError = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}

function requestDelete(entry: OutboundProxyEntry): void {
  deleteTarget = entry
  deleteOpen = true
}

async function confirmDelete(): Promise<void> {
  const target = deleteTarget
  if (!target || deletingId !== null || !proxyQuery.data) return
  deletingId = target.id
  try {
    const nextProxies = proxies.filter((item) => item.id !== target.id)
    await saveProxyList(nextProxies)
    clearProbe(target.id)
    deleteOpen = false
    deleteTarget = null
    toast.success(m.settings_proxy_deleted())
  } catch (cause) {
    deleteOpen = false
    deleteTarget = null
    toast.error(localizeBackendErrorMessage(cause))
  } finally {
    deletingId = null
  }
}
</script>

{#snippet probeButton(loading: boolean, disabled: boolean, onclick: () => void)}
  <Button type="button" variant="ghost" size="sm" {disabled} aria-busy={loading} {onclick}>
    {#if loading}
      <Spinner data-icon="inline-start" />{m.settings_proxy_probing()}
    {:else}
      <LocateFixedIcon data-icon="inline-start" />{m.settings_proxy_probe_ip()}
    {/if}
  </Button>
{/snippet}

{#snippet probeResults(probe: ProbeState | undefined)}
  {#if probe?.loading}
    <p class="mt-1.5 flex items-center gap-1.5 text-xs text-muted-foreground" role="status">
      <Spinner class="size-3.5 shrink-0" aria-hidden="true" />
      <span>{m.settings_proxy_probing_ai_services()}</span>
    </p>
  {:else if probe?.error}
    <p class="mt-1.5 flex items-start gap-1.5 text-xs text-destructive" role="alert">
      <TriangleAlertIcon class="mt-px size-3.5 shrink-0" aria-hidden="true" />
      <span class="min-w-0 break-words">{probe.error}</span>
    </p>
  {:else if probe?.result}
    {@const result = probe.result}
    {#if result.groups.length === 0}
      <div class="mt-1.5 flex flex-wrap items-center gap-1.5 text-xs text-destructive" role="status">
        <TriangleAlertIcon class="size-3.5 shrink-0" aria-hidden="true" />
        {#if result.failed_services.length > 0}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="cursor-default rounded-sm text-left underline decoration-destructive/45 underline-offset-2 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:decoration-destructive focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50">
              {m.settings_proxy_probe_all_failed({ count: result.failed_services.length })}
            </Tooltip.Trigger>
            <Tooltip.Content side="top" sideOffset={6} class="max-w-72 flex-col items-start gap-1.5 py-2">
              <p class="text-[11px] font-medium">{m.settings_proxy_probe_failed_services()}</p>
              <ul class="flex flex-wrap gap-x-2.5 gap-y-0.5 font-technical text-[11px] leading-snug opacity-90">
                {#each result.failed_services as service (service)}
                  <li>{service}</li>
                {/each}
              </ul>
            </Tooltip.Content>
          </Tooltip.Root>
        {:else}
          <span>{m.settings_proxy_probe_all_failed({ count: result.total_services })}</span>
        {/if}
      </div>
    {:else}
      <div class="mt-1.5 flex flex-wrap items-center gap-1.5" role="status">
        {#each result.groups as group (group.ip)}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="inline-flex max-w-full cursor-default items-center gap-1.5 rounded-md border border-transparent bg-muted/60 px-2 py-0.5 text-xs text-foreground transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:border-border/80 hover:bg-muted/85 focus-visible:border-ring focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50 dark:bg-muted/40 dark:hover:bg-muted/60">
              <CountryFlag code={group.country_code} />
              <span class="truncate font-technical tabular-nums">{group.ip}</span>
              {#if group.services.length > 1}
                <span class="shrink-0 font-technical text-[11px] text-muted-foreground tabular-nums">
                  ×{group.services.length}
                </span>
              {/if}
            </Tooltip.Trigger>
            <Tooltip.Content side="top" sideOffset={6} class="max-w-72 flex-col items-start gap-1.5 py-2">
              <div
                class="flex w-full items-center justify-between gap-3 border-b border-background/15 pb-1 text-[11px]">
                <span class="font-medium">
                  {group.services.length === 1
                    ? m.settings_proxy_probe_service_single()
                    : m.settings_proxy_probe_services_heading({ count: group.services.length })}
                </span>
                {#if group.country_code}
                  <span class="font-technical uppercase opacity-75">{group.country_code}</span>
                {/if}
              </div>
              <ul class="flex flex-wrap gap-x-2.5 gap-y-0.5 font-technical text-[11px] leading-snug opacity-95">
                {#each group.services as service (service)}
                  <li>{service}</li>
                {/each}
              </ul>
            </Tooltip.Content>
          </Tooltip.Root>
        {/each}
        {#if result.failed_services.length > 0}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="inline-flex cursor-default items-center gap-1 rounded-sm px-1.5 py-0.5 text-xs text-warning underline decoration-warning/40 underline-offset-2 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:decoration-warning focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50">
              <TriangleAlertIcon class="size-3 shrink-0" aria-hidden="true" />
              <span class="tabular-nums">
                {m.settings_proxy_probe_failed_count({ count: result.failed_services.length })}
              </span>
            </Tooltip.Trigger>
            <Tooltip.Content side="top" sideOffset={6} class="max-w-72 flex-col items-start gap-1.5 py-2">
              <p class="text-[11px] font-medium">{m.settings_proxy_probe_failed_services()}</p>
              <ul class="flex flex-wrap gap-x-2.5 gap-y-0.5 font-technical text-[11px] leading-snug opacity-90">
                {#each result.failed_services as service (service)}
                  <li>{service}</li>
                {/each}
              </ul>
            </Tooltip.Content>
          </Tooltip.Root>
        {/if}
      </div>
    {/if}
  {/if}
{/snippet}

<SettingsSection id="proxy" title={m.settings_proxy()}>
  {#snippet action()}
    <Button type="button" variant="outline" size="sm" disabled={!proxyQuery.data} onclick={openAdd}>
      <PlusIcon data-icon="inline-start" />{m.settings_proxy_add()}
    </Button>
  {/snippet}

  <div class="@container/proxy-list min-w-0">
    <!-- Top row: system proxy summary + inline IP probe -->
    <div
      class="px-4 py-3 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:bg-muted/35 dark:hover:bg-muted/20">
      <div
        class="flex min-w-0 flex-col gap-2 @min-[30rem]/proxy-list:flex-row @min-[30rem]/proxy-list:items-start @min-[30rem]/proxy-list:justify-between @min-[30rem]/proxy-list:gap-4 @min-[34rem]/proxy-list:items-center">
        <div
          class="flex min-w-0 flex-1 flex-col gap-0.5 @min-[34rem]/proxy-list:flex-row @min-[34rem]/proxy-list:items-center @min-[34rem]/proxy-list:gap-3">
          <div class="flex shrink-0 flex-wrap items-center gap-2">
            <span class="text-sm leading-snug font-medium text-foreground">{m.settings_proxy_system_title()}</span>
            {#if systemProxy}
              <Badge variant="secondary">{systemSourceLabel}</Badge>
            {/if}
          </div>
          {#if systemProxyQuery.isPending}
            <Skeleton class="mt-0.5 h-4 w-40 shrink-0 @min-[34rem]/proxy-list:mt-0" />
          {:else if systemProxyQuery.error}
            <div class="flex min-w-0 flex-wrap items-center gap-1.5 text-xs text-destructive">
              <span>{m.settings_proxy_system_load_failed()}</span>
              <Button
                type="button"
                variant="ghost"
                size="xs"
                disabled={systemProxyQuery.isFetching}
                onclick={() => void systemProxyQuery.refetch()}>
                {m.common_retry()}
              </Button>
            </div>
          {:else if systemDetectedUrl}
            <div class="flex min-w-0 @min-[34rem]/proxy-list:flex-1">
              <TechnicalValue value={systemDetectedUrl} class="block text-muted-foreground" />
            </div>
          {:else}
            <span class="min-w-0 truncate text-xs text-muted-foreground" title={m.proxy_system_direct()}>
              {m.proxy_system_direct()}
            </span>
          {/if}
        </div>
        <div class="flex shrink-0 items-center gap-1">
          {@render probeButton(Boolean(systemProbe?.loading), Boolean(systemProbe?.loading), () => {
            void probeTarget(PROXY_MODE_SYSTEM)
          })}
          <span class="hidden size-7 shrink-0 @min-[30rem]/proxy-list:block" aria-hidden="true"></span>
        </div>
      </div>
      {@render probeResults(systemProbe)}
    </div>

    <!-- Saved proxies list -->
    {#if proxyQuery.error}
      <div class="border-t border-border/60 p-4">
        <RequestFailure
          title={m.settings_some_settings_not_loaded()}
          message={localizeBackendErrorMessage(proxyQuery.error)}
          retry={() => void proxyQuery.refetch()}
          retrying={proxyQuery.isFetching} />
      </div>
    {:else if proxyQuery.isPending}
      <div class="flex flex-col gap-3 border-t border-border/60 px-4 py-3" aria-busy="true">
        <Skeleton class="h-7" />
      </div>
    {:else if proxies.length === 0}
      <div class="flex min-h-13 flex-wrap items-center justify-between gap-3 border-t border-border/60 px-4 py-3">
        <p class="text-sm text-muted-foreground">{m.settings_proxy_list_empty()}</p>
      </div>
    {:else}
      {#each proxies as entry (entry.id)}
        {@const named = entry.name.trim().length > 0}
        {@const displayName = proxyDisplayName(entry)}
        {@const redactedUrl = redactProxyUrl(entry.url)}
        {@const showSecondaryUrl = named && redactedUrl.length > 0 && displayName !== redactedUrl}
        {@const probe = probes[entry.id]}
        {@const isDeleting = deletingId === entry.id}
        <div
          class="border-t border-border/60 px-4 py-3 transition-colors duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:bg-muted/35 dark:hover:bg-muted/20">
          <div
            class="flex min-w-0 flex-col gap-2 @min-[30rem]/proxy-list:flex-row @min-[30rem]/proxy-list:items-start @min-[30rem]/proxy-list:justify-between @min-[30rem]/proxy-list:gap-4 @min-[34rem]/proxy-list:items-center">
            <div
              class="flex min-w-0 flex-1 flex-col gap-0.5 @min-[34rem]/proxy-list:flex-row @min-[34rem]/proxy-list:items-center @min-[34rem]/proxy-list:gap-3">
              {#if showSecondaryUrl}
                <span
                  class="min-w-0 truncate text-sm leading-snug font-medium text-foreground @min-[34rem]/proxy-list:max-w-[45%]"
                  title={displayName}>
                  {displayName}
                </span>
                <div class="flex min-w-0 @min-[34rem]/proxy-list:flex-1">
                  <TechnicalValue value={redactedUrl} class="block text-muted-foreground" />
                </div>
              {:else}
                <div class="flex min-w-0 flex-1">
                  <TechnicalValue
                    value={redactedUrl || displayName}
                    class="block text-sm leading-snug font-medium text-foreground" />
                </div>
              {/if}
            </div>
            <div class="flex shrink-0 items-center gap-1">
              {@render probeButton(Boolean(probe?.loading), Boolean(probe?.loading) || isDeleting, () => {
                void probeTarget(entry.id)
              })}
              <DropdownMenu.Root>
                <DropdownMenu.Trigger disabled={isDeleting}>
                  {#snippet child({ props })}
                    <Button
                      {...props}
                      type="button"
                      variant="ghost"
                      size="icon-sm"
                      disabled={isDeleting}
                      aria-busy={isDeleting}
                      aria-label={m.common_more_actions()}>
                      {#if isDeleting}
                        <Spinner aria-hidden="true" />
                      {:else}
                        <MoreHorizontalIcon />
                      {/if}
                    </Button>
                  {/snippet}
                </DropdownMenu.Trigger>
                <DropdownMenu.Content align="end" class="w-36">
                  <DropdownMenu.Group>
                    <DropdownMenu.Item disabled={isDeleting} onSelect={() => openEdit(entry)}>
                      {m.common_edit()}
                    </DropdownMenu.Item>
                  </DropdownMenu.Group>
                  <DropdownMenu.Separator />
                  <DropdownMenu.Group>
                    <DropdownMenu.Item
                      variant="destructive"
                      disabled={isDeleting}
                      onSelect={() => requestDelete(entry)}>
                      {m.settings_proxy_delete()}
                    </DropdownMenu.Item>
                  </DropdownMenu.Group>
                </DropdownMenu.Content>
              </DropdownMenu.Root>
            </div>
          </div>
          {@render probeResults(probe)}
        </div>
      {/each}
    {/if}
  </div>
</SettingsSection>

<SettingsSheet
  bind:open={editorOpen}
  title={editingId ? m.settings_edit_proxy() : m.settings_proxy_add()}
  draft={() => ({ name: nameDraft, url: urlDraft })}
  submitLabel={editingId ? m.settings_save_proxy() : m.settings_proxy_add()}
  submitDisabled={!urlDraft.trim()}
  {saving}
  error={editorError}
  onsubmit={submitEditor}
  onclose={() => (editorError = '')}>
  <Field.FieldGroup>
    <Field.Field>
      <Field.FieldLabel for="proxy-name" hint={m.settings_proxy_name_hint()}>
        {m.settings_proxy_name()}
      </Field.FieldLabel>
      <Input
        id="proxy-name"
        bind:value={nameDraft}
        disabled={saving}
        placeholder={m.settings_proxy_name_placeholder()}
        autocomplete="off" />
    </Field.Field>
    <Field.Field>
      <Field.FieldLabel for="proxy-url" hint={m.settings_proxy_url_hint()}>
        {m.settings_proxy_url()}
      </Field.FieldLabel>
      <Input
        id="proxy-url"
        class="font-technical"
        bind:value={urlDraft}
        required
        disabled={saving}
        placeholder="http://127.0.0.1:7890"
        autocomplete="off" />
    </Field.Field>
  </Field.FieldGroup>
</SettingsSheet>

<AlertDialog.Root
  bind:open={
    () => deleteOpen,
    (next: boolean) => {
      if (deletingId === null) deleteOpen = next
    }
  }>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.settings_proxy_delete_title()}</AlertDialog.Title>
      <AlertDialog.Description>
        {m.settings_proxy_delete_description({ name: deleteTarget ? proxyDisplayName(deleteTarget) : '' })}
      </AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel disabled={deletingId !== null}>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action
        class={buttonVariants({ variant: 'destructive' })}
        disabled={deletingId !== null}
        onclick={() => void confirmDelete()}>
        {#if deletingId !== null}<Spinner data-icon="inline-start" />{/if}{m.settings_proxy_delete()}
      </AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
