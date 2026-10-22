<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { onDestroy, untrack } from 'svelte'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { formatDuration } from '#lib/format.js'
import { localeState } from '#lib/localization.svelte.js'
import { resolvePluginText } from '#lib/plugin-text.js'
import { ProviderConnectionDraft, type ProviderConnectionSubmission } from '#lib/provider-connection-draft.svelte.js'
import type { Provider, UpdateProvider } from '#lib/types.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import FormSection from '#lib/components/form-section.svelte'
import ProviderConfigFields from '#lib/components/provider-config-fields.svelte'
import ProviderOAuthAuthorization from '#lib/components/provider-oauth-authorization.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

interface Props {
  provider: Provider
  onSaved?: (provider: Provider) => void
}

let { provider, onSaved }: Props = $props()
const initialProvider = untrack(() => provider)
const queryClient = useQueryClient()
let testing = $state(false)
let connectionError = $state('')

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

function updateProvider(submission: ProviderConnectionSubmission): Promise<Provider> {
  const input: UpdateProvider = {
    name: submission.name,
    base_url: submission.baseUrl,
    use_proxy: submission.useProxy,
    vendor_options: submission.options,
    ...(Object.keys(submission.credentials).length > 0 ? { adapter_credentials: submission.credentials } : {}),
  }

  return admin.providers.update(provider.id, input)
}

const draft = new ProviderConnectionDraft({
  fields: {
    name: initialProvider.name,
    baseUrl: initialProvider.base_url,
    protocol: initialProvider.protocol,
    useProxy: initialProvider.use_proxy,
    values: { ...(initialProvider.vendor_options ?? {}) },
  },

  target: () => (descriptor && channel ? { kind: 'edit', provider, descriptor, channel } : undefined),
  api: admin.providers,
  writer: { write: updateProvider, bind: (saved, sessionId) => admin.providers.bindOAuth(saved.id, sessionId) },
  oauth: {
    cancel: () => oauthAuthorization?.cancel() ?? Promise.resolve(),
    consume: () => oauthAuthorization?.consume(),
    updateProxy: (useProxy: boolean) => oauthAuthorization?.updateProxy(useProxy) ?? Promise.resolve(),
  },
  hooks: {
    onSaved: async (saved) => {
      baseline.reset()
      connectionError = ''
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['providers'] }),
        queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
      ])
      toast.success(m.provider_connection_view_connection_settings_saved())
      onSaved?.(saved)
    },
    onPartialSave: (saved) => {
      baseline.reset()
      void Promise.all([
        queryClient.invalidateQueries({ queryKey: ['providers'] }),
        queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
      ]).catch((error) => toast.error(localizeBackendErrorMessage(error)))

      onSaved?.(saved)
    },
  },
})

const previewIssues = $derived(draft.preview?.issues ?? [])
const previewIssueError = $derived(
  previewIssues
    .filter((issue) => !issue.field)
    .map((issue) => resolvePluginText(issue.message, localeState.current))
    .join(' ') || (previewIssues.length > 0 ? m.provider_config_fix_issues_before_save() : ''),
)

const submitMessage = $derived(
  draft.partialSaveError !== undefined
    ? m.provider_config_saved_auth_bind_failed({ error: localizeBackendErrorMessage(draft.partialSaveError) })
    : draft.submitError !== undefined
      ? localizeBackendErrorMessage(draft.submitError)
      : '',
)

// 草稿离开保护：路由离开、刷新与关页在有未保存变更时先确认；提交期间（preview → write → bind）锁定。
const baseline = new DraftBaseline(() => draft.fields)
const guard = new DraftGuard({
  dirty: () => baseline.dirty || Boolean(draft.oauthSessionId),
  busy: () => draft.submitting,
})

onDestroy(() => draft.dispose())

$effect(() => {
  if (!descriptor) return
  const additions: Record<string, unknown> = {}
  for (const field of descriptor.config_fields) {
    if (!field.secret && !(field.key in draft.fields.values) && field.default_json != null) {
      additions[field.key] = field.default_json
    } else if (!field.secret && !(field.key in draft.fields.values) && field.required && field.kind.type === 'bool') {
      additions[field.key] = false
    }
  }
  if (Object.keys(additions).length > 0) {
    // 补齐默认值不是用户改动：基线干净时一并更新基线。
    const clean = untrack(() => !baseline.dirty)
    draft.fields.values = { ...additions, ...draft.fields.values }
    if (clean) untrack(() => baseline.reset())
  }
})

function save(): void {
  connectionError = ''
  void draft.submit()
}

function nameChanged(): void {
  connectionError = ''
  draft.nameChanged()
}

function configurationChanged(): void {
  connectionError = ''
  draft.configurationChanged()
}

function useProxyChanged(checked: boolean): void {
  connectionError = ''
  draft.fields.useProxy = checked
  draft.useProxyChanged()
}

function oauthStateChanged(sessionId: string | undefined, ready: boolean): void {
  connectionError = ''
  draft.oauthStateChanged(sessionId, ready)
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
</script>

<section class="route-section" aria-labelledby="provider-connection-title">
  <!-- 页签已经写明「连接设置」，可见标题只会重复；保留语义标题供辅助技术定位。 -->
  <h2 id="provider-connection-title" class="sr-only">{m.provider_connection_view_connection_settings()}</h2>

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
    class="flex flex-col gap-6"
    onsubmit={(event) => {
      event.preventDefault()
      save()
    }}>
    <FormSection title={m.provider_connection_view_section_connection()}>
      <Field.Group class="grid gap-4 sm:grid-cols-2">
        <Field.Field size="name" class="sm:col-span-2">
          <Field.Label for="provider-name">{m.common_connection_name()}</Field.Label>
          <Input
            id="provider-name"
            bind:value={draft.fields.name}
            required
            disabled={draft.submitting}
            oninput={nameChanged} />
        </Field.Field>
        <Field.Field size="fill" class="sm:col-span-2">
          <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
          <Input
            id="provider-base-url"
            class="font-technical"
            bind:value={draft.fields.baseUrl}
            type="url"
            required={!supportsConfigValidation}
            disabled={draft.submitting}
            oninput={configurationChanged} />
        </Field.Field>
        {#if descriptor && channel}
          <ProviderConfigFields
            fields={configFields}
            configGroups={descriptor.config_groups}
            protocol={provider.protocol}
            bind:values={draft.fields.values}
            {configuredSecretFields}
            issues={previewIssues}
            idPrefix={`provider-config-${provider.id}`}
            disabled={draft.submitting}
            onChanged={configurationChanged} />
        {/if}
      </Field.Group>

      {#if draft.unknownOptionKeys.length > 0}
        <Alert.Root>
          <Alert.Title>{m.provider_config_legacy_values_retained()}</Alert.Title>
          <Alert.Description>
            {m.provider_config_legacy_values_retained_help({ fields: draft.unknownOptionKeys.join(', ') })}
          </Alert.Description>
        </Alert.Root>
      {/if}

      {#if oauthProvider && descriptor && channel}
        <ProviderOAuthAuthorization
          bind:this={oauthAuthorization}
          vendorId={descriptor.provider_id}
          channel={channel.id}
          flow={channel.auth!.flow}
          configuration={draft.oauthConfiguration}
          useProxy={draft.fields.useProxy}
          mode="reconnect"
          disabled={draft.submitting}
          onStateChange={oauthStateChanged} />
      {/if}
    </FormSection>

    <FormSection title={m.provider_connection_view_section_network()}>
      <Field.Field orientation="horizontal" class="justify-between">
        <Field.Content>
          <Field.Label for="provider-use-proxy">{m.common_use_proxy()}</Field.Label>
          <Field.Description>{m.common_send_requests_service_proxy_configured_settings()}</Field.Description>
        </Field.Content>
        <Switch
          id="provider-use-proxy"
          checked={draft.fields.useProxy}
          disabled={draft.submitting}
          onCheckedChange={useProxyChanged} />
      </Field.Field>
    </FormSection>

    {#if connectionError || submitMessage || previewIssueError}
      <p class="text-sm text-destructive">{connectionError || submitMessage || previewIssueError}</p>
    {/if}
    <div class="flex flex-wrap justify-end gap-2 border-t pt-4">
      {#if supportsInference}
        <Button
          type="button"
          variant="outline"
          onclick={() => void testConnection()}
          disabled={testing || draft.submitting}>
          {#if testing}<Spinner data-icon="inline-start" />{/if}{m.providers_test_connection()}
        </Button>
      {/if}
      <Button type="submit" disabled={draft.submitting || !draft.submittable}>
        {#if draft.submitting}<Spinner data-icon="inline-start" />{/if}
        {m.provider_connection_view_save_connection()}
      </Button>
    </div>
  </form>
</section>

<DraftDiscardDialog {guard} />
