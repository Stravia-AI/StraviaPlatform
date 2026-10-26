<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import { onDestroy, untrack } from 'svelte'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import { localeState } from '#lib/localization.svelte.js'
import { resolvePluginText } from '#lib/plugin-text.js'
import {
  mergeProviderValidationIssues,
  providerConnectionCheckBaseUrlIssues,
  providerConnectionCheckConfigurationIssues,
  providerConnectionCheckFailureMessage,
  providerConnectionCheckFieldIssues,
  providerConnectionCheckSuccessMessage,
  providerConnectionCheckUnsupportedMessage,
} from '#lib/provider-connection-check.js'
import { ProviderConnectionDraft, type ProviderConnectionSubmission } from '#lib/provider-connection-draft.svelte.js'
import type { Provider, ProviderValidationIssue, UpdateProvider } from '#lib/types.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import FormSection from '#lib/components/form-section.svelte'
import ProviderConfigFields from '#lib/components/provider-config-fields.svelte'
import ProviderConnectionStatus from '#lib/components/provider-connection-status.svelte'
import ProviderOAuthAuthorization from '#lib/components/provider-oauth-authorization.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import SettingRow from '#lib/components/setting-row.svelte'
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
let connectionNotice = $state('')
let testIssues = $state<ProviderValidationIssue[]>([])
let testGeneration = 0

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
const supportsDiscovery = $derived(channel?.capabilities.includes('model_discovery') ?? false)
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
      invalidateConnectionCheck()
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['providers'] }),
        queryClient.invalidateQueries({ queryKey: ['provider-models', provider.id] }),
      ])
      toast.success(m.provider_connection_view_connection_settings_saved())
      onSaved?.(saved)
    },
    onPartialSave: (saved) => {
      baseline.reset()
      invalidateConnectionCheck()
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

const connectionIssues = $derived(mergeProviderValidationIssues(previewIssues, testIssues))
const baseUrlIssues = $derived(providerConnectionCheckBaseUrlIssues(connectionIssues))
const configurationIssues = $derived(providerConnectionCheckConfigurationIssues(connectionIssues))

const submitMessage = $derived(
  draft.partialSaveError !== undefined
    ? m.provider_config_saved_auth_bind_failed({ error: localizeBackendErrorMessage(draft.partialSaveError) })
    : draft.submitError !== undefined
      ? localizeBackendErrorMessage(draft.submitError)
      : '',
)

// 草稿离开保护：路由离开、刷新与关页在有未保存变更时先确认；提交期间（preview → write → bind）锁定。
const baseline = new DraftBaseline(() => draft.fields)
const guard = new DraftGuard({ dirty: () => baseline.dirty, busy: () => draft.submitting })

onDestroy(() => {
  invalidateConnectionCheck()
  draft.dispose()
})

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

/**
 * 草稿、目标或 OAuth 快照变化后，进行中的检查结果不得再写回当前界面。
 * 递增代际同时收起旧反馈，允许用户立即用新草稿重新检查。
 */
function invalidateConnectionCheck(): void {
  testGeneration += 1
  testing = false
  testIssues = []
  connectionError = ''
  connectionNotice = ''
}

$effect(() => {
  void provider.id
  void provider.updated_at
  void descriptor
  untrack(invalidateConnectionCheck)
})

// 取消：把草稿还原为已保存的连接并放弃进行中的授权；有改动时先经过离开确认。
function revert(): void {
  invalidateConnectionCheck()
  void draft
    .resetConnection({
      name: provider.name,
      baseUrl: provider.base_url,
      protocol: provider.protocol,
      useProxy: provider.use_proxy,
      values: { ...(provider.vendor_options ?? {}) },
    })
    .then(() => baseline.reset())
}

function save(): void {
  invalidateConnectionCheck()
  void draft.submit()
}

function nameChanged(): void {
  invalidateConnectionCheck()
  draft.nameChanged()
}

function configurationChanged(): void {
  invalidateConnectionCheck()
  draft.configurationChanged()
}

function useProxyChanged(checked: boolean): void {
  invalidateConnectionCheck()
  draft.fields.useProxy = checked
  draft.useProxyChanged()
}

function oauthStateChanged(sessionId: string | undefined, ready: boolean): void {
  invalidateConnectionCheck()
  draft.oauthStateChanged(sessionId, ready)
}

async function testConnection(): Promise<void> {
  if (testing || draft.submitting || !descriptor || !channel) return
  const input = draft.connectionCheckInput({ kind: 'edit', provider, descriptor, channel })
  invalidateConnectionCheck()
  testing = true
  const generation = testGeneration
  try {
    const result = await admin.providers.test(provider.id, input)
    if (generation !== testGeneration) return
    if (result.status === 'passed') {
      toast.success(providerConnectionCheckSuccessMessage(result))
    } else if (result.status === 'unsupported') {
      connectionNotice = providerConnectionCheckUnsupportedMessage()
    } else if (result.status === 'failed') {
      testIssues = providerConnectionCheckFieldIssues(result)
      connectionError = providerConnectionCheckFailureMessage(result)
    }
  } catch (error) {
    if (generation !== testGeneration) return
    connectionError = localizeBackendErrorMessage(error)
  } finally {
    if (generation === testGeneration) testing = false
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

  <!-- 表单始终占居中的设置列；容器足够宽时辅助栏落在表单右侧的空白里，没有内容就不占位，较窄时排在表单之后。 -->
  <div
    class="mx-auto grid w-full max-w-(--content-column-max-width) gap-6 @min-[96rem]/route-page:max-w-none @min-[96rem]/route-page:grid-cols-[minmax(0,1fr)_minmax(0,var(--content-column-max-width))_minmax(0,1fr)] @min-[96rem]/route-page:items-start @min-[96rem]/route-page:gap-x-12">
    <form
      class="@container/provider-form flex min-w-0 flex-col gap-6 @min-[96rem]/route-page:col-start-2 @min-[96rem]/route-page:row-start-1"
      onsubmit={(event) => {
        event.preventDefault()
        save()
      }}>
      <FormSection title={m.provider_connection_view_section_connection()}>
        <!-- 表单列够宽（≥42rem）时名称与 Base URL 同行：名称取 16rem，地址占余下宽度。 -->
        <Field.Group
          class="@2xl/provider-form:grid @2xl/provider-form:grid-cols-[minmax(0,16rem)_minmax(0,1fr)] @2xl/provider-form:gap-x-6">
          <Field.Field>
            <Field.Label for="provider-name">{m.common_connection_name()}</Field.Label>
            <Input
              id="provider-name"
              bind:value={draft.fields.name}
              required
              disabled={draft.submitting}
              oninput={nameChanged} />
          </Field.Field>
          <Field.Field data-invalid={baseUrlIssues.length > 0 || undefined}>
            <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
            <Input
              id="provider-base-url"
              class="font-technical"
              bind:value={draft.fields.baseUrl}
              type="url"
              required={!supportsConfigValidation}
              disabled={draft.submitting}
              aria-invalid={baseUrlIssues.length > 0 || undefined}
              oninput={configurationChanged} />
            {#each baseUrlIssues as issue, index (`${issue.field}:${issue.code}:${index}`)}
              <Field.Error>{resolvePluginText(issue.message, localeState.current)}</Field.Error>
            {/each}
          </Field.Field>
          {#if descriptor && channel}
            <ProviderConfigFields
              fields={configFields}
              configGroups={descriptor.config_groups}
              protocol={provider.protocol}
              bind:values={draft.fields.values}
              {configuredSecretFields}
              issues={configurationIssues}
              idPrefix={`provider-config-${provider.id}`}
              vendorName={descriptor.display_name}
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
        <!-- 代理开关属于连接草稿：随「保存连接」一起提交，不即时生效。 -->
        <SettingRow
          title={m.common_use_proxy()}
          labelFor="provider-use-proxy"
          description={m.common_send_requests_service_proxy_configured_settings()}
          layout="inline">
          <Switch
            id="provider-use-proxy"
            checked={draft.fields.useProxy}
            disabled={draft.submitting}
            onCheckedChange={useProxyChanged} />
        </SettingRow>
      </FormSection>

      {#if connectionError || submitMessage || previewIssueError || connectionNotice}
        <p
          class={connectionError || submitMessage || previewIssueError
            ? 'text-sm text-destructive'
            : 'text-sm text-muted-foreground'}
          role={connectionError || submitMessage || previewIssueError ? 'alert' : 'status'}>
          {connectionError || submitMessage || previewIssueError || connectionNotice}
        </p>
      {/if}
      <!-- 固定操作区：在文档流中保留空间，不覆盖最后一个字段；取消在前、保存在后。 -->
      <div
        class="sticky bottom-0 z-20 flex translate-y-2 flex-wrap justify-end gap-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
        {#if descriptor && channel}
          <Button
            type="button"
            variant="outline"
            class="me-auto"
            data-provider-connection-check
            onclick={() => void testConnection()}
            disabled={testing || draft.submitting}
            aria-busy={testing}
            aria-describedby="provider-connection-check-hint">
            {#if testing}<Spinner data-icon="inline-start" />{/if}
            <!-- 保留按钮宽度：检查中标签替换为「测试中…」，原标签只占位不朗读。 -->
            <span class="grid">
              <span class="col-start-1 row-start-1 {testing ? 'invisible' : ''}">{m.providers_test_connection()}</span>
              {#if testing}
                <span class="col-start-1 row-start-1">{m.providers_connection_check_testing()}</span>
              {/if}
            </span>
          </Button>
          <span id="provider-connection-check-hint" class="sr-only">{m.providers_connection_check_draft_hint()}</span>
        {/if}
        <Button
          type="button"
          variant="outline"
          onclick={() => guard.request(revert)}
          disabled={draft.submitting || (!baseline.dirty && !draft.oauthSessionId)}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={draft.submitting || !draft.submittable}>
          {#if draft.submitting}<Spinner data-icon="inline-start" />{/if}
          {m.provider_connection_view_save_connection()}
        </Button>
      </div>
    </form>
    <ProviderConnectionStatus {provider} oauth={oauthProvider} discovery={supportsDiscovery} />
  </div>
</section>

<DraftDiscardDialog {guard} />
