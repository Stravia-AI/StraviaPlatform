<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatDuration } from '$lib/format'
import type { OAuthCandidateConfiguration, Provider, ProviderConfigurationPreview, UpdateProvider } from '$lib/types'
import ProviderConfigFields from '$lib/components/provider-config-fields.svelte'
import ProviderOAuthAuthorization from '$lib/components/provider-oauth-authorization.svelte'
import * as Alert from '$lib/components/ui/alert'
import { Badge } from '$lib/components/ui/badge'
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
let reviewing = $state(false)
let reviewRequestId = 0
let connectionError = $state('')
let preview = $state<ProviderConfigurationPreview>()
let previewFailure = $state('')
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
const globalIssues = $derived(previewIssues.filter((issue) => !issue.field))
const previewAccepted = $derived(Boolean(preview) && previewIssues.length === 0)
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
  reviewRequestId += 1
  reviewing = false
  preview = undefined
  previewFailure = ''
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

async function reviewConfiguration(): Promise<void> {
  if (!descriptor || !channel || (!form.baseUrl.trim() && !supportsConfigValidation)) return
  reviewRequestId += 1
  const requestId = reviewRequestId
  reviewing = true
  previewFailure = ''
  preview = undefined
  try {
    const result = await admin.providers.previewConfiguration({
      provider_id: provider.id,
      vendor_id: descriptor.provider_id,
      channel: channel.id,
      base_url: form.baseUrl.trim(),
      options: configurationValues(false),
      credentials: configurationValues(true),
    })
    if (requestId !== reviewRequestId) return
    preview = result
    if (result.issues.length === 0) form.baseUrl = result.base_url
  } catch (error) {
    if (requestId === reviewRequestId) previewFailure = localizeBackendErrorMessage(error)
  } finally {
    if (requestId === reviewRequestId) reviewing = false
  }
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
  if (!descriptor || !channel || unknownOptionKeys.length > 0 || !form.name.trim() || !previewAccepted || !preview) {
    return
  }
  if (oauthSessionId && !oauthReady) return
  const credentials = configurationValues(true)
  const input: UpdateProvider = {
    name: form.name.trim(),
    base_url: preview.base_url,
    use_proxy: form.useProxy,
    vendor_options: configurationValues(false),
    ...(Object.keys(credentials).length > 0 ? { adapter_credentials: credentials } : {}),
  }
  saving = true
  connectionError = ''
  try {
    const saved = await admin.providers.update(provider.id, input)
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
    ])
    if (oauthSessionId) {
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
        <Field.Field class="sm:col-span-2">
          <Field.Label>{m.provider_config_channel_capabilities()}</Field.Label>
          <div class="flex flex-wrap gap-1.5">
            {#each channel.capabilities as capability (capability)}
              <Badge variant="secondary" class="font-technical">{capability}</Badge>
            {/each}
          </div>
        </Field.Field>
        <ProviderConfigFields
          fields={configFields}
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

    <section class="rounded-xl border p-4" aria-labelledby="provider-network-review-title">
      <div class="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 id="provider-network-review-title" class="font-medium">{m.provider_config_review_title()}</h3>
          <p class="mt-1 text-sm text-muted-foreground">{m.provider_config_review_help()}</p>
        </div>
        <Button
          type="button"
          variant="outline"
          disabled={reviewing ||
            !descriptor ||
            !channel ||
            (!form.baseUrl.trim() && !supportsConfigValidation) ||
            unknownOptionKeys.length > 0}
          onclick={() => void reviewConfiguration()}>
          {#if reviewing}<Spinner data-icon="inline-start" />{/if}
          {m.provider_config_review_action()}
        </Button>
      </div>
      {#if preview}
        <dl class="mt-4 flex flex-col gap-3">
          <div>
            <dt class="text-xs text-muted-foreground">{m.provider_config_saved_base_url()}</dt>
            <dd class="font-technical mt-1 break-all text-sm">{preview.base_url}</dd>
          </div>
          <div>
            <dt class="text-xs text-muted-foreground">{m.provider_config_authorized_origins()}</dt>
            <dd class="mt-1">
              {#if preview.network_permissions.length > 0}
                <ul class="flex flex-col gap-1">
                  {#each preview.network_permissions as permission (`${permission.origin}:${permission.configuration_field ?? ''}:${permission.connection_scoped}`)}
                    <li class="font-technical break-all text-sm">
                      {permission.origin}
                      {#if permission.configuration_field}
                        <span class="font-sans text-xs text-muted-foreground">
                          · {m.provider_config_origin_from_field({ field: permission.configuration_field })}
                        </span>
                      {:else if permission.connection_scoped}
                        <span class="font-sans text-xs text-muted-foreground">
                          · {m.provider_config_connection_scoped_origin()}
                        </span>
                      {/if}
                    </li>
                  {/each}
                </ul>
              {:else}
                <span class="text-sm text-muted-foreground">{m.provider_config_no_origins()}</span>
              {/if}
            </dd>
          </div>
        </dl>
        {#if globalIssues.length > 0}
          <ul class="mt-4 flex list-disc flex-col gap-1 pl-5 text-sm text-destructive">
            {#each globalIssues as issue (`${issue.code}:${issue.message}`)}<li>{issue.message}</li>{/each}
          </ul>
        {:else if previewAccepted}
          <p class="mt-4 text-sm text-success">{m.provider_config_review_ready()}</p>
        {/if}
      {:else}
        <p class="mt-4 text-sm text-muted-foreground">{m.provider_config_review_required()}</p>
      {/if}
      {#if previewFailure}<p class="mt-3 text-sm text-destructive">{previewFailure}</p>{/if}
    </section>

    {#if connectionError}<p class="text-sm text-destructive">{connectionError}</p>{/if}
    <div class="flex flex-wrap justify-end gap-2 border-t pt-4">
      {#if supportsInference}
        <Button type="button" variant="outline" onclick={() => void testConnection()} disabled={testing || saving}>
          {#if testing}<Spinner data-icon="inline-start" />{/if}{m.providers_test_connection()}
        </Button>
      {/if}
      <Button
        type="submit"
        disabled={saving ||
          !descriptor ||
          !channel ||
          unknownOptionKeys.length > 0 ||
          !form.name.trim() ||
          !previewAccepted ||
          (Boolean(oauthSessionId) && !oauthReady)}>
        {#if saving}<Spinner data-icon="inline-start" />{/if}
        {m.provider_connection_view_save_connection()}
      </Button>
    </div>
  </form>
</section>
