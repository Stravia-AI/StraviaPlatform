<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import SaveIcon from '@lucide/svelte/icons/save'
import { setMode, userPrefersMode } from 'mode-watcher'
import { toast } from 'svelte-sonner'
import { onMount } from 'svelte'

import { admin, isTauri } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import DesktopPortSettings from '$lib/components/desktop-port-settings.svelte'
import LanguageSelector from '$lib/components/language-selector.svelte'
import PageHeader from '$lib/components/page-header.svelte'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

const queryClient = useQueryClient()
const statusQuery = createQuery(() => ({
  queryKey: ['gateway-status'],
  queryFn: admin.settings.status,
}))
const retentionQuery = createQuery(() => ({
  queryKey: ['setting', 'log_retention_days'],
  queryFn: () => admin.settings.get('log_retention_days'),
}))
const proxyEnabledQuery = createQuery(() => ({
  queryKey: ['setting', 'proxy_enabled'],
  queryFn: () => admin.settings.get('proxy_enabled'),
}))
const proxyUrlQuery = createQuery(() => ({
  queryKey: ['setting', 'proxy_url'],
  queryFn: () => admin.settings.get('proxy_url'),
}))
const proxyBypassQuery = createQuery(() => ({
  queryKey: ['setting', 'proxy_bypass'],
  queryFn: () => admin.settings.get('proxy_bypass'),
}))
let editedRetention = $state<string>()
let proxyDraft = $state<{ enabled: boolean; url: string; bypass: string }>()
let savingRetention = $state(false)
let savingProxy = $state(false)

const retentionBaseline = $derived((retentionQuery.data ?? '7').trim())
const retention = $derived(editedRetention ?? retentionBaseline)
const retentionDirty = $derived(editedRetention != null && retention.trim() !== retentionBaseline)
const storedProxy = $derived({
  enabled: ['1', 'true', 'yes', 'on'].includes((proxyEnabledQuery.data ?? '').trim().toLowerCase()),
  url: proxyUrlQuery.data ?? '',
  bypass: proxyBypassQuery.data ?? '',
})
const proxy = $derived(proxyDraft ?? storedProxy)
const proxyDirty = $derived(
  proxyDraft != null &&
    (proxy.enabled !== storedProxy.enabled ||
      proxy.url.trim() !== storedProxy.url.trim() ||
      proxy.bypass.trim() !== storedProxy.bypass.trim()),
)
const settingsError = $derived(
  retentionQuery.error ?? proxyEnabledQuery.error ?? proxyUrlQuery.error ?? proxyBypassQuery.error,
)
const appVersion = $derived(statusQuery.data?.version ?? '–')
const currentTheme = $derived(userPrefersMode.current ?? 'system')

function scrollToSection(id: string): void {
  const section = document.getElementById(id)

  const scrollContainer = section?.closest<HTMLElement>('.shell-scrollbar')
  if (!section || !scrollContainer) return

  const scrollMarginTop = Number.parseFloat(getComputedStyle(section).scrollMarginTop) || 0
  scrollContainer.scrollTop +=
    section.getBoundingClientRect().top - scrollContainer.getBoundingClientRect().top - scrollMarginTop
}

function handleThemeValueChange(value: string): void {
  if (value === 'system' || value === 'light' || value === 'dark') setMode(value)
}

onMount(() => {
  if (!isTauri || window.location.hash !== '#desktop') return
  requestAnimationFrame(() => scrollToSection('desktop'))
})

async function saveSetting(key: string, value: string): Promise<void> {
  await admin.settings.set(key, value)
  await queryClient.invalidateQueries({ queryKey: ['setting', key] })
}

async function saveRetention(): Promise<void> {
  const value = Number.parseInt(retention, 10)
  if (!Number.isInteger(value) || value < 1 || value > 365) {
    toast.error(m.settings_retention_must_whole_number_between_1_365())
    return
  }
  savingRetention = true
  try {
    await saveSetting('log_retention_days', String(value))
    queryClient.setQueryData(['setting', 'log_retention_days'], String(value))
    editedRetention = undefined
    toast.success(m.settings_request_history_settings_saved())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    savingRetention = false
  }
}

async function saveProxy(): Promise<void> {
  const url = proxy.url.trim()
  const bypass = proxy.bypass.trim()
  if (proxy.enabled && !url) {
    toast.error(m.settings_set_proxy_url_enabling_proxy())
    return
  }
  savingProxy = true
  try {
    await Promise.all([
      saveSetting('proxy_enabled', proxy.enabled ? 'true' : 'false'),
      saveSetting('proxy_url', url),
      saveSetting('proxy_bypass', bypass),
    ])
    queryClient.setQueryData(['setting', 'proxy_enabled'], proxy.enabled ? 'true' : 'false')
    queryClient.setQueryData(['setting', 'proxy_url'], url)
    queryClient.setQueryData(['setting', 'proxy_bypass'], bypass)
    proxyDraft = undefined
    toast.success(m.settings_proxy_settings_saved())
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    savingProxy = false
  }
}

function retrySettings(): void {
  void Promise.all([
    retentionQuery.refetch(),
    proxyEnabledQuery.refetch(),
    proxyUrlQuery.refetch(),
    proxyBypassQuery.refetch(),
  ])
}
</script>

<svelte:head><title>{m.settings_settings()} · Stravia</title></svelte:head>

<div class="route-page mx-auto max-w-[64rem]">
  <PageHeader eyebrow={m.common_system_label()} title={m.settings_settings()} description={m.settings_page_summary()} />

  {#if settingsError}
    <div class="border-y py-4">
      <p class="text-sm font-medium text-destructive">
        {m.settings_some_settings_not_loaded()}
      </p>
      <p class="mt-1 text-sm text-muted-foreground">
        {localizeBackendErrorMessage(settingsError)}
      </p>
      <Button class="mt-3" variant="outline" onclick={retrySettings}>{m.common_retry()}</Button>
    </div>
  {/if}

  <div class="min-w-0">
    <section id="appearance" class="scroll-mt-20 pb-8" aria-labelledby="appearance-title">
      <div class="route-section-header">
        <div>
          <h2 id="appearance-title" class="route-section-title">{m.settings_appearance()}</h2>
        </div>
      </div>
      <Field.FieldGroup>
        <Field.Field size="select"
          ><Field.FieldLabel for="theme-preference">{m.settings_theme()}</Field.FieldLabel><Select.Root
            type="single"
            value={currentTheme}
            onValueChange={handleThemeValueChange}
            ><Select.Trigger id="theme-preference" class="w-full" aria-label={m.settings_theme_preference()}>
              {currentTheme === 'system'
                ? m.settings_follow_system_label()
                : currentTheme === 'light'
                  ? m.settings_light()
                  : m.settings_dark()}
            </Select.Trigger>
            <Select.Content>
              <Select.Group>
                <Select.Item value="system" label={m.settings_follow_system_label()}>
                  {m.settings_follow_system_label()}
                </Select.Item>
                <Select.Item value="light" label={m.settings_light()}>{m.settings_light()}</Select.Item>
                <Select.Item value="dark" label={m.settings_dark()}>{m.settings_dark()}</Select.Item>
              </Select.Group>
            </Select.Content>
          </Select.Root></Field.Field>
        <LanguageSelector description={false} />
      </Field.FieldGroup>
    </section>

    {#if isTauri}<DesktopPortSettings />{/if}

    <section id="proxy" class="route-section scroll-mt-20 pb-8" aria-labelledby="proxy-title">
      <div class="route-section-header">
        <div>
          <h2 id="proxy-title" class="route-section-title">{m.settings_proxy()}</h2>
          <p class="route-section-description">
            {m.settings_send_model_service_requests_proxy()}
          </p>
        </div>
      </div>
      <Field.FieldGroup>
        <Field.Field orientation="horizontal"
          ><div class="flex-1">
            <Field.FieldLabel for="proxy-enabled">{m.settings_outbound_proxy()}</Field.FieldLabel>
          </div>
          <Switch
            id="proxy-enabled"
            checked={proxy.enabled}
            onCheckedChange={(enabled) => (proxyDraft = { ...proxy, enabled })}
            disabled={savingProxy} /></Field.Field>
        <Field.Field size="fill"
          ><Field.FieldLabel for="proxy-url">{m.settings_proxy_url()}</Field.FieldLabel><Input
            id="proxy-url"
            class="font-technical"
            value={proxy.url}
            oninput={(event) => (proxyDraft = { ...proxy, url: event.currentTarget.value })}
            placeholder="http://127.0.0.1:7890" /></Field.Field>
        <Field.Field
          ><Field.FieldLabel for="proxy-bypass">{m.settings_bypass_hosts_optional()}</Field.FieldLabel><Input
            id="proxy-bypass"
            value={proxy.bypass}
            oninput={(event) => (proxyDraft = { ...proxy, bypass: event.currentTarget.value })}
            placeholder="localhost,127.0.0.1,.internal" /></Field.Field>
        <div class="field-actions">
          <Button disabled={!proxyDirty || savingProxy} onclick={() => void saveProxy()}
            >{#if savingProxy}<Spinner data-icon="inline-start" />{:else}<SaveIcon
                data-icon="inline-start" />{/if}{m.settings_save_proxy()}</Button>
        </div>
      </Field.FieldGroup>
    </section>

    <section id="logs" class="route-section scroll-mt-20 pb-8" aria-labelledby="logs-title">
      <div class="route-section-header">
        <div>
          <h2 id="logs-title" class="route-section-title">{m.common_request_history()}</h2>
          <p class="route-section-description">
            {m.settings_request_history_summary()}
          </p>
        </div>
      </div>
      <Field.FieldGroup>
        <Field.Field size="number"
          ><Field.FieldLabel for="log-retention" hint={m.settings_automatically_deletes_older_request_history()}
            >{m.settings_retention_period_days()}</Field.FieldLabel
          ><Input
            id="log-retention"
            type="number"
            min="1"
            max="365"
            value={retention}
            oninput={(event) => (editedRetention = event.currentTarget.value)} /></Field.Field>
        <div class="field-actions">
          <Button disabled={!retentionDirty || savingRetention} onclick={() => void saveRetention()}
            >{#if savingRetention}<Spinner data-icon="inline-start" />{:else}<SaveIcon
                data-icon="inline-start" />{/if}{m.settings_save_request_history()}</Button>
        </div>
      </Field.FieldGroup>
    </section>

    <p class="font-technical pt-8 text-center text-xs text-muted-foreground">
      {m.settings_version()}
      {appVersion}
    </p>
  </div>
</div>
