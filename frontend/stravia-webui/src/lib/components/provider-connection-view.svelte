<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatDuration } from '$lib/format'
import { localeState } from '$lib/localization.svelte'
import { resolvePluginText } from '$lib/plugin-text'
import type { OAuthCandidateConfiguration, Provider, ProviderConfigurationPreview, UpdateProvider } from '$lib/types'
import ProviderConfigFields from '$lib/components/provider-config-fields.svelte'
import ProviderOAuthAuthorization from '$lib/components/provider-oauth-authorization.svelte'
import * as Alert from '$lib/components/ui/alert'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

interface Props {
  provider: Provider
  onSaved?: (provider: Provider) => void
}

let { provider, onSaved }: Props = $props()
const initialProvider = untrack(() => provider)
const queryClient = useQueryClient()
let form = $state({
  name: initialProvider.name,
  baseUrl: initialProvider.base_url,
  useProxy: initialProvider.use_proxy,
  values: { ...(initialProvider.vendor_options ?? {}) },
})
let saving = $state(false)
let testing = $state(false)
let saveGeneration = 0
let connectionError = $state('')
let preview = $state<ProviderConfigurationPreview>()
let oauthSessionId = $state<string>()
let oauthReady = $state(false)
let oauthAuthorization = $state<{
  cancel: () => Promise<void>
  consume: () => void
  updateProxy: (useProxy: boolean) => Promise<void>
}>()

const providerDescriptorsQuery = createQuery(() => ({
  queryKey: ['provider-descriptors'],
  queryFn: admin.providers.descriptors,
}))
const vendorId = $derived(provider.vendor ?? '')
const descriptor = $derived(providerDescriptorsQuery.data?.find((item) => item.provider_id === vendorId))
const channel = $derived(descriptor?.channels.find((item) => item.id === provider.channel))
const configFields = $derived(descriptor?.config_fields ?? [])
const configuredSecretFields = $derived(provider.configured_credential_fields ?? [])
const oauthProvider = $derived(Boolean(channel?.auth))
const supportsInference = $derived(channel?.capabilities.includes('infer') ?? false)
const supportsConfigValidation = $derived(channel?.capabilities.includes('config_validation') ?? false)
const unknownOptionKeys = $derived(
  Object.keys(initialProvider.vendor_options ?? {}).filter(
    (key) => !configFields.some((field) => !field.secret && field.key === key),
  ),
)
const previewIssues = $derived(preview?.issues ?? [])
const previewIssueError = $derived(
  previewIssues
    .filter((issue) => !issue.field)
    .map((issue) => resolvePluginText(issue.message, localeState.current))
    .join(' ') || (previewIssues.length > 0 ? m.provider_config_fix_issues_before_save() : ''),
)
const oauthConfiguration = $derived.by((): OAuthCandidateConfiguration => ({
  provider_id: provider.id,
  base_url: form.baseUrl.trim(),
  protocol: provider.protocol || undefined,
  options: configurationValues(false),
  credentials: configurationValues(true),
}))

$effect(() => {
  if (!descriptor) return
  const additions: Record<string, unknown> = {}
  for (const field of descriptor.config_fields) {
    if (!field.secret && !(field.key in form.values) && field.default_json != null) {
      additions[field.key] = field.default_json
    } else if (!field.secret && !(field.key in form.values) && field.required && field.kind.type === 'bool') {
      additions[field.key] = false
    }
  }
  if (Object.keys(additions).length > 0) form.values = { ...additions, ...form.values }
})

function invalidatePreview(): void {
  saveGeneration += 1
  preview = undefined
  connectionError = ''
}

function configurationChanged(): void {
  invalidatePreview()
  if (!oauthSessionId) return
  oauthSessionId = undefined
  oauthReady = false
  void oauthAuthorization?.cancel()
}

function configurationValues(secret: boolean): Record<string, unknown> {
  const declared = new Set(configFields.filter((field) => field.secret === secret).map((field) => field.key))
  const entries = Object.entries(form.values).filter(
    ([key, value]) =>
      declared.has(key) &&
      value !== undefined &&
      (!secret || (value !== null && (typeof value !== 'string' || value.length > 0))),
  )
  if (!secret) {
    for (const [key, value] of Object.entries(initialProvider.vendor_options ?? {})) {
      if (!configFields.some((field) => field.key === key) && !entries.some(([entryKey]) => entryKey === key)) {
        entries.push([key, value])
      }
    }
  }
  return Object.fromEntries(entries)
}

async function testConnection(): Promise<void> {
  testing = true
  connectionError = ''
  try {
    const result = await admin.providers.test(provider.id)
    if (result.success) toast.success(m.common_service_response_time({ duration: formatDuration(result.latency_ms) }))
    else connectionError = result.error || m.common_connection_test_failed()
  } catch (error) {
    connectionError = localizeBackendErrorMessage(error)
  } finally {
    testing = false
  }
}

async function save(): Promise<void> {
  if (saving || !descriptor || !channel || unknownOptionKeys.length > 0 || !form.name.trim()) {
    return
  }
  const generation = saveGeneration
  saving = true
  connectionError = ''
  try {
    let baseUrl = form.baseUrl.trim()
    if (supportsConfigValidation || baseUrl) {
      const result = await admin.providers.previewConfiguration({
        provider_id: provider.id,
        vendor_id: descriptor.provider_id,
        channel: channel.id,
        base_url: baseUrl,
        options: configurationValues(false),
        credentials: configurationValues(true),
      })
      if (generation !== saveGeneration) return
      preview = result
      if (result.issues.length > 0) {
        return
      }
      baseUrl = result.base_url
    }
    const credentials = configurationValues(true)
    const input: UpdateProvider = {
      name: form.name.trim(),
      base_url: baseUrl,
      use_proxy: form.useProxy,
      vendor_options: configurationValues(false),
      ...(Object.keys(credentials).length > 0 ? { adapter_credentials: credentials } : {}),
    }
    const saved = await admin.providers.update(provider.id, input)
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
    ])
    if (oauthSessionId && oauthReady) {
      try {
        await admin.providers.bindOAuth(provider.id, oauthSessionId)
        oauthAuthorization?.consume()
      } catch (error) {
        connectionError = m.provider_config_saved_auth_bind_failed({ error: localizeBackendErrorMessage(error) })
        onSaved?.(saved)
        return
      }
    }
    form.name = saved.name
    form.baseUrl = saved.base_url
    form.values = { ...(saved.vendor_options ?? {}) }
    preview = undefined
    toast.success(m.provider_connection_view_connection_settings_saved())
    onSaved?.(saved)
  } catch (error) {
    connectionError = localizeBackendErrorMessage(error)
  } finally {
    saving = false
  }
}
</script>

<section class="route-section" aria-labelledby="provider-connection-title">
  <div class="route-section-header">
    <div>
      <h2 id="provider-connection-title" class="route-section-title">
        {m.provider_connection_view_connection_settings()}
      </h2>
      <p class="route-section-description">{m.provider_connection_view_summary()}</p>
    </div>
  </div>

  {#if providerDescriptorsQuery.isError}
    <Alert.Root variant="destructive" class="mb-4">
      <Alert.Title>{m.provider_config_plugin_unavailable()}</Alert.Title>
      <Alert.Description>{localizeBackendErrorMessage(providerDescriptorsQuery.error)}</Alert.Description>
    </Alert.Root>
  {:else if !providerDescriptorsQuery.isPending && !descriptor}
    <Alert.Root variant="destructive" class="mb-4">
      <Alert.Title>{m.provider_config_plugin_missing()}</Alert.Title>
      <Alert.Description>{m.provider_config_plugin_missing_help({ vendor: vendorId || '—' })}</Alert.Description>
    </Alert.Root>
  {:else if descriptor && !channel}
    <Alert.Root variant="destructive" class="mb-4">
      <Alert.Title>{m.provider_config_channel_missing()}</Alert.Title>
      <Alert.Description
        >{m.provider_config_channel_missing_help({ channel: provider.channel ?? '—' })}</Alert.Description>
    </Alert.Root>
  {/if}

  <form
    class="flex flex-col gap-5"
    onsubmit={(event) => {
      event.preventDefault()
      void save()
    }}>
    <Field.Group class="grid gap-4 sm:grid-cols-2">
      <Field.Field size="name" class="sm:col-span-2">
        <Field.Label for="provider-name">{m.common_connection_name()}</Field.Label>
        <Input id="provider-name" bind:value={form.name} required oninput={invalidatePreview} />
      </Field.Field>
      <Field.Field size="fill" class="sm:col-span-2">
        <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
        <Input
          id="provider-base-url"
          class="font-technical"
          bind:value={form.baseUrl}
          type="url"
          required={!supportsConfigValidation}
          oninput={configurationChanged} />
      </Field.Field>
      {#if descriptor && channel}
        <ProviderConfigFields
          fields={configFields}
          configGroups={descriptor.config_groups}
          bind:values={form.values}
          {configuredSecretFields}
          issues={previewIssues}
          idPrefix={`provider-config-${provider.id}`}
          onChanged={configurationChanged} />
      {/if}
    </Field.Group>

    {#if unknownOptionKeys.length > 0}
      <Alert.Root>
        <Alert.Title>{m.provider_config_legacy_values_retained()}</Alert.Title>
        <Alert.Description>
          {m.provider_config_legacy_values_retained_help({ fields: unknownOptionKeys.join(', ') })}
        </Alert.Description>
      </Alert.Root>
    {/if}

    {#if oauthProvider && descriptor && channel}
      <ProviderOAuthAuthorization
        bind:this={oauthAuthorization}
        vendorId={descriptor.provider_id}
        channel={channel.id}
        flow={channel.auth!.flow}
        configuration={oauthConfiguration}
        useProxy={form.useProxy}
        mode="reconnect"
        onStateChange={(sessionId: string | undefined, ready: boolean) => {
          oauthSessionId = sessionId
          oauthReady = ready
          invalidatePreview()
          if (sessionId && ready) void save()
        }} />
    {/if}

    <Field.Field orientation="horizontal" class="min-h-10 justify-between rounded-lg border px-3 py-2">
      <Field.Label for="provider-use-proxy" hint={m.common_send_requests_service_proxy_configured_settings()}>
        {m.common_use_proxy()}
      </Field.Label>
      <Switch
        id="provider-use-proxy"
        checked={form.useProxy}
        onCheckedChange={(checked: boolean) => {
          form.useProxy = checked
          invalidatePreview()
          if (oauthReady) {
            oauthSessionId = undefined
            oauthReady = false
          }
          void oauthAuthorization?.updateProxy(checked)
        }} />
    </Field.Field>

    {#if connectionError || previewIssueError}
      <p class="text-sm text-destructive">{connectionError || previewIssueError}</p>
    {/if}
    <div class="flex flex-wrap justify-end gap-2 border-t pt-4">
      {#if supportsInference}
        <Button type="button" variant="outline" onclick={() => void testConnection()} disabled={testing || saving}>
          {#if testing}<Spinner data-icon="inline-start" />{/if}{m.providers_test_connection()}
        </Button>
      {/if}
      <Button
        type="submit"
        disabled={saving || !descriptor || !channel || unknownOptionKeys.length > 0 || !form.name.trim()}>
        {#if saving}<Spinner data-icon="inline-start" />{/if}
        {m.provider_connection_view_save_connection()}
      </Button>
    </div>
  </form>
</section>
