<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { useQueryClient } from '@tanstack/svelte-query'
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'
import { untrack } from 'svelte'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatDuration } from '$lib/format'
import { PROTOCOL_TABLE, resolveProtocol } from '$lib/protocol'
import type { Provider, UpdateProvider } from '$lib/types'
import ProviderOAuthAuthorization from '$lib/components/provider-oauth-authorization.svelte'
import * as Field from '$lib/components/ui/field'
import { Button } from '$lib/components/ui/button'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import { Textarea } from '$lib/components/ui/textarea'

interface Props {
  provider: Provider
  onSaved?: (provider: Provider) => void
}

let { provider, onSaved }: Props = $props()
const initialProvider = untrack(() => provider)
const queryClient = useQueryClient()
let form = $state({
  name: initialProvider.name,
  vendor: initialProvider.vendor ?? '',
  protocol: resolveProtocol(initialProvider.protocol) ?? 'openai-compatible',
  baseUrl: initialProvider.base_url,
  apiKey: '',
  useProxy: initialProvider.use_proxy,
  modelsSource: initialProvider.models_source ?? '',
  staticModels: initialProvider.static_models ?? '',
})
let saving = $state(false)
let testing = $state(false)
let credentialError = $state('')
let showApiKey = $state(false)
let oauthSessionId = $state<string>()
let oauthReady = $state(false)
let oauthAuthorization = $state<{ consume: () => void; updateProxy: (useProxy: boolean) => Promise<void> }>()
const custom = $derived(!provider.preset_key)
const oauthProvider = $derived(provider.auth_mode === 'oauth')

async function testConnection(): Promise<void> {
  testing = true
  credentialError = ''
  try {
    const result = await admin.providers.test(provider.id)
    if (result.success) {
      toast.success(m.common_service_response_time({ duration: formatDuration(result.latency_ms) }))
    } else {
      credentialError = result.error || m.common_connection_test_failed()
    }
  } catch (error) {
    credentialError = localizeBackendErrorMessage(error)
  } finally {
    testing = false
  }
}

async function save(): Promise<void> {
  if (!form.name.trim() || !form.baseUrl.trim()) return
  if (oauthSessionId && !oauthReady) return
  const input: UpdateProvider = {
    name: form.name.trim(),
    use_proxy: form.useProxy,
    api_key: oauthProvider ? undefined : form.apiKey.trim() || undefined,
    ...(custom
      ? {
          vendor: form.vendor.trim() || undefined,
          protocol: form.protocol,
          base_url: form.baseUrl.trim(),
          models_source: form.modelsSource.trim() || undefined,
          static_models: form.staticModels.trim() || undefined,
        }
      : {}),
  }
  saving = true
  credentialError = ''
  try {
    const saved = await admin.providers.update(provider.id, input)
    if (oauthSessionId) {
      await admin.providers.bindOAuth(provider.id, oauthSessionId)
      oauthAuthorization?.consume()
    }
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
    ])
    toast.success(m.provider_connection_view_connection_settings_saved())
    onSaved?.(saved)
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
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
      <p class="route-section-description">
        {m.provider_connection_view_summary()}
      </p>
    </div>
  </div>

  <form
    class="flex flex-col gap-5"
    onsubmit={(event) => {
      event.preventDefault()
      void save()
    }}>
    <div class="grid gap-4 sm:grid-cols-2">
      <Field.Field size="name" class="sm:col-span-2">
        <Field.Label for="provider-name">{m.common_connection_name()}</Field.Label>
        <Input id="provider-name" bind:value={form.name} required />
      </Field.Field>
      <Field.Field size="select">
        <Field.Label for="provider-protocol">{m.common_protocol()}</Field.Label>
        {#if custom}
          <Select.Root type="single" bind:value={form.protocol}>
            <Select.Trigger id="provider-protocol" class="w-full">
              {PROTOCOL_TABLE.find((entry) => entry.id === form.protocol)?.displayName}
            </Select.Trigger>
            <Select.Content>
              {#each PROTOCOL_TABLE as entry (entry.id)}
                <Select.Item value={entry.id}>{entry.displayName}</Select.Item>
              {/each}
            </Select.Content>
          </Select.Root>
        {:else}
          <Input id="provider-protocol" value={provider.protocol} readonly />
        {/if}
      </Field.Field>
      <Field.Field size="fill">
        <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
        <Input
          id="provider-base-url"
          class="font-technical"
          bind:value={form.baseUrl}
          type="url"
          readonly={!custom}
          required />
      </Field.Field>
      {#if custom}
        <Field.Field size="name">
          <Field.Label for="provider-vendor">{m.common_service_identifier()}</Field.Label>
          <Input id="provider-vendor" class="font-technical" bind:value={form.vendor} />
        </Field.Field>
      {/if}
      {#if oauthProvider}
        <ProviderOAuthAuthorization
          class="sm:col-span-2"
          bind:this={oauthAuthorization}
          driver={provider.channel ?? provider.preset_key ?? provider.vendor ?? ''}
          useProxy={form.useProxy}
          mode="reconnect"
          onStateChange={(sessionId, ready) => {
            oauthSessionId = sessionId
            oauthReady = ready
          }} />
      {:else}
        <Field.Field size="fill" data-invalid={credentialError ? true : undefined}>
          <Field.Label for="provider-api-key">{m.common_api_key()}</Field.Label>
          <div class="flex flex-wrap gap-2">
            <Input
              id="provider-api-key"
              class="font-technical min-w-0 flex-1"
              bind:value={form.apiKey}
              type={showApiKey ? 'text' : 'password'}
              autocomplete="off"
              oninput={() => (credentialError = '')}
              placeholder={m.provider_connection_view_leave_blank_keep_current_key()} />
            <Button
              type="button"
              variant="outline"
              size="icon"
              onclick={() => (showApiKey = !showApiKey)}
              aria-label={showApiKey ? m.common_hide_secret() : m.common_show_secret()}>
              {#if showApiKey}<EyeOffIcon />{:else}<EyeIcon />{/if}
            </Button>
            <Button
              type="button"
              variant="outline"
              onclick={() => void testConnection()}
              disabled={testing || saving}>
              {#if testing}<Spinner data-icon="inline-start" />{/if}{m.providers_test_connection()}
            </Button>
          </div>
          {#if credentialError}<Field.Error>{credentialError}</Field.Error>{/if}
        </Field.Field>
      {/if}
      {#if custom}
        <Field.Field>
          <Field.Label for="provider-models-source">{m.common_model_list_url()}</Field.Label>
          <Input id="provider-models-source" class="font-technical" bind:value={form.modelsSource} type="url" />
        </Field.Field>
        <Field.Field>
          <Field.Label for="provider-static-models">{m.common_additional_model_ids()}</Field.Label>
          <Textarea id="provider-static-models" class="min-h-28 font-technical" bind:value={form.staticModels} />
        </Field.Field>
      {/if}
    </div>
    <div class="flex min-h-10 items-center justify-between gap-3 rounded-lg border px-3 py-2">
      <div>
        <Field.Label for="provider-use-proxy" hint={m.common_send_requests_service_proxy_configured_settings()}>
          {m.common_use_proxy()}
        </Field.Label>
      </div>
      <Switch
        id="provider-use-proxy"
        checked={form.useProxy}
        onCheckedChange={(checked) => {
          form.useProxy = checked
          void oauthAuthorization?.updateProxy(checked)
        }} />
    </div>
    <div class="flex justify-end border-t pt-4">
      <Button
        type="submit"
        disabled={saving || !form.name.trim() || !form.baseUrl.trim() || (Boolean(oauthSessionId) && !oauthReady)}>
        {#if saving}<Spinner data-icon="inline-start" />{/if}
        {m.provider_connection_view_save_connection()}
      </Button>
    </div>
  </form>
</section>
