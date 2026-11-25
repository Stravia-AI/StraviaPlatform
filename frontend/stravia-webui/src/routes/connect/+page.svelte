<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import RequestFailure from '#lib/components/request-failure.svelte'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { resolve } from '$app/paths'
import { afterNavigate, goto, snapshot } from '$app/navigation'
import { page } from '$app/state'
import { untrack } from 'svelte'
import type { MouseEventHandler } from 'svelte/elements'
import { createQuery } from '@tanstack/svelte-query'
import BlocksIcon from '@lucide/svelte/icons/blocks'
import ClipboardCopyIcon from '@lucide/svelte/icons/clipboard-copy'
import CheckIcon from '@lucide/svelte/icons/check'
import Code2Icon from '@lucide/svelte/icons/code-2'
import TerminalSquareIcon from '@lucide/svelte/icons/terminal-square'
import { toast } from 'svelte-sonner'

import { admin, capabilitiesQueryOptions, isTauri, proxyBase } from '#lib/admin-client.js'
import { getConnectSetup } from '#lib/connect-setup.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { copyText } from '#lib/clipboard.js'
import { applyConnectClient, asConnectClientApplyError, planConnectClient } from '#lib/connect-client-apply.js'
import { ensureBuildExtensionsRegistered } from '#lib/extensions/index.js'
import { logicalModelOption, sortLogicalModels } from '#lib/logical-model.js'
import { syncQueryParams } from '#lib/url-query-nav.js'
import { parseEnumParam } from '#lib/url-query-state.js'
import {
  apiKeyAllowsModel,
  apiKeyReadOnlyReason,
  buildCode,
  CLI_TOOLS,
  defineClientModel,
  eligibleConnectKeys,
  maskApiKey,
  type ClaudeModelMappings,
  type CliToolId,
  type CodeLanguage,
  type ConnectClientApplyRequest,
  type GatewayProtocol,
} from '#lib/connect.js'
import ConnectControlRow from '#lib/components/connect-control-row.svelte'
import ExtensionSlot from '#lib/components/extension-slot.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import SearchCombobox from '#lib/components/search-combobox.svelte'
import StatusIndicator from '#lib/components/status-indicator.svelte'
import TechnicalValue from '#lib/components/technical-value.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import * as Empty from '#lib/components/ui/empty/index.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Select from '#lib/components/ui/select/index.js'
import { Skeleton } from '#lib/components/ui/skeleton/index.js'
import * as Tabs from '#lib/components/ui/tabs/index.js'

const CONNECT_TABS = ['cli', 'code', 'mcp'] as const
type ConnectTab = (typeof CONNECT_TABS)[number]

interface ConnectSnapshot {
  codeLanguage: CodeLanguage
  codeProtocol: GatewayProtocol
  codeModelId: string
  codeKeyId: string
  cliToolId: CliToolId
  cliKeyId: string
  claudeModelIds: Record<keyof ClaudeModelMappings, string>
  mcpKeyId: string
  mcpSnippetFormat: 'json' | 'curl'
}

const emptyKey = 'sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'
const setup = getConnectSetup()
const initial = untrack(() => setup.draft)
setup.draft = undefined
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const proxyQuery = createQuery(() => ({
  queryKey: ['proxy-base'],
  queryFn: proxyBase,
  staleTime: Number.POSITIVE_INFINITY,
}))
const capabilitiesQuery = createQuery(() => capabilitiesQueryOptions())
const capabilities = $derived(capabilitiesQuery.data?.capabilities ?? [])
const extensions = ensureBuildExtensionsRegistered()
const hasConnectMcpSlot = $derived(
  extensions.activeSlots(capabilities, 'connect:mcp-endpoints').some((item) => typeof item.component === 'function'),
)
const currentUrl = $derived(page.shallow?.url ?? page.url)
let initialTabOverride = $state<ConnectTab | undefined>(initial?.tab)
let syncedInitialTab = false
let tab = $derived<ConnectTab>(
  initialTabOverride ?? parseEnumParam(currentUrl.searchParams, 'tab', CONNECT_TABS, 'cli'),
)
let codeLanguage = $state<CodeLanguage>(initial?.codeLanguage ?? 'python')
let codeProtocol = $state<GatewayProtocol>(initial?.codeProtocol ?? 'openai-compatible')
let codeModelId = $state(initial?.codeModelId ?? '')
let codeKeyId = $state(initial?.codeKeyId ?? '')
let cliToolId = $state<CliToolId>(initial?.cliToolId ?? 'codex-cli')
let cliKeyId = $state(initial?.cliKeyId ?? '')
let mcpKeyId = $state(initial?.mcpKeyId ?? '')
let mcpSnippetFormat = $state<'json' | 'curl'>(initial?.mcpSnippetFormat ?? 'json')
let applyingClient = $state(false)
let claudeModelIds = $state<Record<keyof ClaudeModelMappings, string>>(
  initial?.claudeModelIds ?? { defaultModel: '', haikuModel: '', sonnetModel: '', opusModel: '' },
)

afterNavigate(() => {
  if (syncedInitialTab || !initialTabOverride) return
  syncedInitialTab = true
  const target = initialTabOverride
  void syncQueryParams({ tab: target }, { defaults: { tab: 'cli' }, shallow: false }).finally(() => {
    const syncedUrl = page.shallow?.url ?? page.url
    if (
      initialTabOverride === target &&
      (syncedUrl.searchParams.get('tab') === target ||
        (target === 'cli' && parseEnumParam(syncedUrl.searchParams, 'tab', CONNECT_TABS, 'cli') === 'cli'))
    ) {
      initialTabOverride = undefined
    }
  })
})

function selectTab(next: string): void {
  if (!(CONNECT_TABS as readonly string[]).includes(next)) return
  const target = next as ConnectTab
  syncedInitialTab = true
  initialTabOverride = undefined
  tab = target
  void syncQueryParams({ tab: target }, { defaults: { tab: 'cli' }, shallow: false })
}

snapshot<ConnectSnapshot>({
  id: 'connect',
  capture: () => ({
    codeLanguage,
    codeProtocol,
    codeModelId,
    codeKeyId,
    cliToolId,
    cliKeyId,
    claudeModelIds: { ...claudeModelIds },
    mcpKeyId,
    mcpSnippetFormat,
  }),
  restore: (value) => {
    if (initial) return
    codeLanguage = value.codeLanguage
    codeProtocol = value.codeProtocol
    codeModelId = value.codeModelId
    codeKeyId = value.codeKeyId
    cliToolId = value.cliToolId
    cliKeyId = value.cliKeyId
    claudeModelIds = { ...value.claudeModelIds }
    mcpKeyId = value.mcpKeyId
    mcpSnippetFormat = value.mcpSnippetFormat
  },
})

const models = $derived(
  sortLogicalModels(
    (modelsQuery.data ?? []).filter(
      (model) => model.is_enabled && (model.purpose === 'inference' || model.purpose === 'image_generation'),
    ),
  ),
)
const apiKeys = $derived(keysQuery.data ?? [])
const cliKeys = $derived(eligibleConnectKeys(apiKeys, models))
const host = $derived(proxyQuery.data ?? window.location.origin)
const resourcesPending = $derived(modelsQuery.isPending || keysQuery.isPending || proxyQuery.isPending)
const resourceError = $derived(modelsQuery.error ?? keysQuery.error ?? proxyQuery.error)
const blockingResourceError = $derived(
  (modelsQuery.data === undefined ? modelsQuery.error : null) ??
    (keysQuery.data === undefined ? keysQuery.error : null) ??
    (proxyQuery.data === undefined ? proxyQuery.error : null),
)
const codeModel = $derived(models.find((model) => model.id === codeModelId))
const codeKeys = $derived(codeModel ? eligibleConnectKeys(apiKeys, [codeModel]) : [])
const selectedCodeKey = $derived(
  codeKeys.find((key) => key.id === codeKeyId) ?? (codeKeys.length === 1 ? codeKeys[0] : undefined),
)
const selectedCliKey = $derived(
  cliKeys.find((key) => key.id === cliKeyId) ?? (cliKeys.length === 1 ? cliKeys[0] : undefined),
)
const cliModels = $derived(
  selectedCliKey ? models.filter((model) => apiKeyAllowsModel(selectedCliKey.model_ids, model.id)) : [],
)
const selectedTool = $derived(CLI_TOOLS.find((tool) => tool.id === cliToolId) ?? CLI_TOOLS[0])
const codeApiKey = $derived(selectedCodeKey?.key ?? emptyKey)
const clientConfigModels = $derived(cliModels.map(defineClientModel))
const claudeMappings = $derived.by((): ClaudeModelMappings | undefined => {
  const mappings = {
    defaultModel: cliModels.find((model) => model.id === claudeModelIds.defaultModel)?.model_id,
    haikuModel: cliModels.find((model) => model.id === claudeModelIds.haikuModel)?.model_id,
    sonnetModel: cliModels.find((model) => model.id === claudeModelIds.sonnetModel)?.model_id,
    opusModel: cliModels.find((model) => model.id === claudeModelIds.opusModel)?.model_id,
  }
  if (!mappings.defaultModel || !mappings.haikuModel || !mappings.sonnetModel || !mappings.opusModel) return undefined
  return mappings as ClaudeModelMappings
})
const connectClientInput = $derived.by((): ConnectClientApplyRequest | undefined => {
  if (!selectedCliKey || clientConfigModels.length === 0) return undefined
  if (cliToolId === 'claude-code') {
    if (!claudeMappings) return undefined
    return { tool: cliToolId, host, apiKey: selectedCliKey.key, models: clientConfigModels, mappings: claudeMappings }
  }
  return {
    tool: cliToolId,
    host,
    apiKey: selectedCliKey.key,
    models: clientConfigModels,
    transparentImageInputEnabled:
      selectedCliKey.transparent_injection_enabled &&
      selectedCliKey.transparent_injections.includes('media_understanding'),
  }
})
const connectPlanQuery = createQuery(() => ({
  queryKey: [
    'connect-client-plan',
    cliToolId,
    selectedCliKey?.id,
    host,
    clientConfigModels,
    claudeMappings,
    selectedCliKey?.transparent_injection_enabled,
    selectedCliKey?.transparent_injections.includes('media_understanding'),
  ],
  queryFn: () => planConnectClient(connectClientInput!),
  enabled: connectClientInput !== undefined,
}))
const generatedCode = $derived(
  buildCode({
    protocol: codeProtocol,
    model: codeModel?.model_id ?? 'gpt-4o',
    apiKey: codeApiKey,
    host,
    language: codeLanguage,
  }),
)
const generatedCliConfig = $derived.by(() => {
  if (!connectClientInput) return ''
  return connectPlanQuery.data?.preview ?? ''
})

const mcpActiveKeys = $derived(apiKeys.filter((key) => apiKeyReadOnlyReason(key) === null))
const selectedMcpKey = $derived(
  mcpActiveKeys.find((key) => key.id === mcpKeyId) ?? (mcpActiveKeys.length === 1 ? mcpActiveKeys[0] : undefined),
)
const builtinMcpUrl = $derived(`${host.replace(/\/+$/, '')}/mcp`)
const generatedMcpSnippet = $derived.by(() => {
  const token = selectedMcpKey?.key || '<YOUR_STRAVIA_API_KEY>'
  if (mcpSnippetFormat === 'json') {
    return JSON.stringify(
      { mcpServers: { stravia: { url: builtinMcpUrl, headers: { Authorization: `Bearer ${token}` } } } },
      null,
      2,
    )
  }
  const payload = JSON.stringify({
    jsonrpc: '2.0',
    id: 1,
    method: 'initialize',
    params: {
      protocolVersion: '2025-03-26',
      capabilities: {},
      clientInfo: { name: 'stravia-curl-check', version: '1.0' },
    },
  })
  return [
    `curl -sS -X POST '${builtinMcpUrl.replaceAll("'", "'\\''")}' \\`,
    `  -H 'Content-Type: application/json' \\`,
    `  -H 'Accept: application/json, text/event-stream' \\`,
    `  -H 'Authorization: Bearer ${token.replaceAll("'", "'\\''")}' \\`,
    `  -d '${payload.replaceAll("'", "'\\''")}'`,
  ].join('\n')
})

const codeProtocols = [
  { id: 'openai-compatible', name: 'OpenAI Compatible', path: '/v1/chat/completions' },
  { id: 'open-responses', name: 'Open Responses', path: '/v1/responses' },
  { id: 'anthropic-messages', name: 'Anthropic Messages', path: '/v1/messages' },
  { id: 'google-gemini', name: 'Google Gemini', path: '/v1beta/models/{model}:generateContent' },
] as const

const claudeMappingRows = [
  { id: 'cli-default-model', key: 'defaultModel', label: () => m.connect_default_model() },
  { id: 'cli-haiku-model', key: 'haikuModel', label: () => m.connect_haiku_model_mapping() },
  { id: 'cli-sonnet-model', key: 'sonnetModel', label: () => m.connect_sonnet_model_mapping() },
  { id: 'cli-opus-model', key: 'opusModel', label: () => m.connect_opus_model_mapping() },
] as const

const copyGeneratedCliConfig: MouseEventHandler<HTMLElement> = ({ currentTarget }) =>
  void copyOutput(generatedCliConfig, currentTarget)
const copyGeneratedCode: MouseEventHandler<HTMLElement> = ({ currentTarget }) =>
  void copyOutput(generatedCode, currentTarget)
const copyGeneratedMcpSnippet: MouseEventHandler<HTMLElement> = ({ currentTarget }) =>
  void copyOutput(generatedMcpSnippet, currentTarget)

async function copyOutput(value: string, trigger: HTMLElement): Promise<void> {
  try {
    await copyText(value, trigger)
    toast.success(m.common_copied_clipboard())
  } catch {
    toast.error(m.common_not_copy_clipboard())
  }
}

async function applySelectedClient(): Promise<void> {
  if (!connectClientInput || !connectPlanQuery.data) return
  applyingClient = true
  try {
    await applyConnectClient(connectClientInput)
    toast.success(m.connect_apply_success_restart({ client: selectedTool.name }))
    await connectPlanQuery.refetch()
  } catch (error) {
    const applyError = asConnectClientApplyError(error)
    toast.error(m.connect_apply_failed({ message: applyError.message }))
  } finally {
    applyingClient = false
  }
}

function retryResources(): void {
  void Promise.all([modelsQuery.refetch(), keysQuery.refetch(), proxyQuery.refetch()])
}

function manageResource(path: '/models' | '/api-keys', createKey = false): void {
  setup.draft = {
    tab,
    codeLanguage,
    codeProtocol,
    codeModelId: codeModel?.id ?? '',
    codeKeyId: selectedCodeKey?.id ?? '',
    cliToolId,
    cliKeyId: selectedCliKey?.id ?? '',
    claudeModelIds: { ...claudeModelIds },
    mcpKeyId: selectedMcpKey?.id ?? '',
    mcpSnippetFormat,
  }
  setup.createKey = createKey
  void goto(resolve(path))
}
</script>

<svelte:head><title>{m.connect_connect_apps()} · Stravia</title></svelte:head>

{#snippet keyActions(selected: boolean)}
  <div class="flex flex-wrap gap-2">
    {#if selected}
      {#if apiKeys.length > 0}
        <Button variant="ghost" size="sm" onclick={() => manageResource('/api-keys')}
          >{m.connect_manage_api_keys()}</Button>
      {/if}
      <Button variant="ghost" size="sm" onclick={() => manageResource('/api-keys', true)}
        >{m.connect_create_an_api_key()}</Button>
    {:else}
      <Button variant="outline" onclick={() => manageResource('/api-keys', true)}
        >{m.connect_create_an_api_key()}</Button>
      {#if apiKeys.length > 0}
        <Button variant="ghost" onclick={() => manageResource('/api-keys')}>{m.connect_manage_api_keys()}</Button>
      {/if}
    {/if}
  </div>
{/snippet}

<div class="@container/connect">
  <div class="route-page connect-page">
    <PageHeader title={m.connect_connect_apps()} />

    {#if resourcesPending}
      <div class="grid w-full max-w-72 grid-cols-2 gap-1" aria-label={m.connect_loading_output_modes()}>
        <Skeleton class="h-10" /><Skeleton class="h-10" />
      </div>
      <div class="grid gap-6 @min-[68rem]/connect:grid-cols-[minmax(18rem,24rem)_minmax(0,1fr)]">
        <Skeleton class="h-96" />
        <Skeleton class="h-96" />
      </div>
    {:else if blockingResourceError}
      <RequestFailure
        title={m.connect_connection_setup_unavailable()}
        message={localizeBackendErrorMessage(blockingResourceError)}
        retry={retryResources}
        retrying={modelsQuery.isFetching || keysQuery.isFetching || proxyQuery.isFetching} />
    {:else}
      {#if resourceError}
        <RequestFailure
          message={m.connect_stale_data_warning()}
          retry={retryResources}
          retrying={modelsQuery.isFetching || keysQuery.isFetching || proxyQuery.isFetching} />
      {/if}

      <Tabs.Root bind:value={() => tab, selectTab} class="min-h-0 flex-1">
        <Tabs.List class="shrink-0" aria-label={m.connect_setup_format()}>
          <Tabs.Trigger value="cli"
            ><TerminalSquareIcon data-icon="inline-start" />{m.connect_clients_label()}</Tabs.Trigger>
          <Tabs.Trigger value="code"><Code2Icon data-icon="inline-start" />{m.connect_code()}</Tabs.Trigger>
          <Tabs.Trigger value="mcp"><BlocksIcon data-icon="inline-start" />{m.connect_mcp_tab()}</Tabs.Trigger>
        </Tabs.List>

        <Tabs.Content value="cli" class="mt-6 min-h-0 data-active:flex data-active:flex-col">
          <div
            class="grid gap-6 @min-[68rem]/connect:min-h-0 @min-[68rem]/connect:flex-1 @min-[68rem]/connect:grid-cols-[minmax(18rem,24rem)_minmax(0,1fr)] @min-[68rem]/connect:grid-rows-[minmax(0,1fr)]">
            <section
              class="min-w-0 @min-[68rem]/connect:min-h-0 @min-[68rem]/connect:overflow-y-auto"
              aria-labelledby="cli-controls-title">
              <h2 id="cli-controls-title" class="sr-only">{m.connect_client_setup()}</h2>
              <Field.FieldGroup>
                <ConnectControlRow label={m.connect_client()} labelFor="cli-tool" hint={selectedTool.description()}>
                  <Select.Root type="single" bind:value={cliToolId}>
                    <Select.Trigger id="cli-tool" class="w-full min-w-0"
                      ><span class="truncate">{selectedTool.name}</span></Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each CLI_TOOLS as tool (tool.id)}<Select.Item value={tool.id} label={tool.name}
                            >{tool.name}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                </ConnectControlRow>
                <ConnectControlRow
                  label={m.common_api_key()}
                  labelFor="cli-key"
                  hint={cliToolId === 'claude-code'
                    ? m.connect_model_mapping_scope()
                    : m.connect_all_api_key_models_included()}>
                  <Select.Root
                    type="single"
                    bind:value={
                      () => selectedCliKey?.id ?? '',
                      (value: string) => {
                        cliKeyId = value
                      }
                    }
                    disabled={cliKeys.length === 0}>
                    <Select.Trigger id="cli-key" class="w-full min-w-0"
                      ><span class="truncate"
                        >{selectedCliKey
                          ? `${selectedCliKey.name} · ${maskApiKey(selectedCliKey.key)}`
                          : m.connect_select_api_key()}</span
                      ></Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each cliKeys as key (key.id)}<Select.Item value={key.id} label={key.name}
                            >{key.name} · {maskApiKey(key.key)}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                  {#snippet below()}
                    {@render keyActions(selectedCliKey !== undefined)}
                  {/snippet}
                </ConnectControlRow>
                {#if selectedCliKey && cliModels.length > 0 && cliToolId === 'claude-code'}
                  {#each claudeMappingRows as row (row.id)}
                    <ConnectControlRow label={row.label()} labelFor={row.id}>
                      <SearchCombobox
                        id={row.id}
                        value={claudeModelIds[row.key]}
                        options={cliModels.map(logicalModelOption)}
                        placeholder={m.common_select_model()}
                        emptyText={m.common_no_matching_models()}
                        onValueChange={(value: string) => (claudeModelIds[row.key] = value)} />
                    </ConnectControlRow>
                  {/each}
                {/if}
              </Field.FieldGroup>
            </section>

            <section class="flex min-h-0 min-w-0 flex-col" aria-labelledby="cli-output-title">
              <div class="mb-3.5 flex shrink-0 flex-wrap items-start justify-between gap-4">
                <h2 id="cli-output-title" class="route-section-title font-structural">
                  {m.providers_connection_check_stage_configuration()}
                </h2>
                <div class="flex flex-col items-start gap-2">
                  <div class="flex flex-wrap gap-2">
                    {#if isTauri}
                      <Button
                        onclick={() => void applySelectedClient()}
                        disabled={!connectClientInput || !connectPlanQuery.data || applyingClient}
                        >{#if applyingClient}<Spinner data-icon="inline-start" />{:else}<CheckIcon
                            data-icon="inline-start" />{/if}{applyingClient
                          ? m.connect_applying()
                          : cliToolId === 'chatgpt'
                            ? m.connect_apply_both()
                            : m.connect_apply()}</Button>
                    {/if}
                    <Button
                      variant={isTauri ? 'outline' : 'default'}
                      onclick={copyGeneratedCliConfig}
                      disabled={!generatedCliConfig}
                      ><ClipboardCopyIcon data-icon="inline-start" />{cliToolId === 'chatgpt'
                        ? m.connect_copy_both()
                        : m.connect_copy_configuration()}</Button>
                  </div>
                  {#if cliToolId === 'chatgpt'}
                    <p class="text-sm">{m.connect_chatgpt_shared_config()}</p>
                  {/if}
                </div>
              </div>
              {#if connectPlanQuery.isError}
                {@const planError = asConnectClientApplyError(connectPlanQuery.error)}
                <RequestFailure
                  class="mb-3 shrink-0"
                  title={m.connect_apply_plan_failed()}
                  message={planError.message}
                  retry={() => connectPlanQuery.refetch()}
                  retrying={connectPlanQuery.isFetching}>
                  {#if planError.path}<p class="font-technical break-all">{planError.path}</p>{/if}
                </RequestFailure>
              {/if}
              {#if generatedCliConfig}
                <pre class="route-code-plane min-h-96 flex-1">{generatedCliConfig}</pre>
              {:else if !connectPlanQuery.isError}
                <Empty.Root class="min-h-72 border-y"
                  ><Empty.Header
                    >{#if models.length === 0}
                      <Empty.Title>{m.connect_add_model_first()}</Empty.Title>
                      <Empty.Description
                        >{m.connect_connect_model_service_add_model_generating_code()}</Empty.Description>
                    {:else if cliKeys.length === 0}
                      <Empty.Title>{m.connect_no_eligible_key()}</Empty.Title>
                      <Empty.Description
                        >{apiKeys.length
                          ? m.connect_no_eligible_key_description()
                          : m.connect_create_key_description()}</Empty.Description>
                    {:else if !selectedCliKey}
                      <Empty.Title>{m.connect_select_api_key()}</Empty.Title>
                      <Empty.Description>{m.connect_select_api_key_client_description()}</Empty.Description>
                    {:else if cliModels.length === 0}
                      <Empty.Title>{m.connect_api_key_has_no_models()}</Empty.Title>
                      <Empty.Description>{m.connect_api_key_has_no_models_description()}</Empty.Description>
                    {:else if cliToolId === 'claude-code' && !claudeMappings}
                      <Empty.Title>{m.connect_complete_model_mappings()}</Empty.Title>
                      <Empty.Description>{m.connect_complete_model_mappings_description()}</Empty.Description>
                    {:else}
                      <Empty.Title>{m.connect_loading_global_config()}</Empty.Title>
                    {/if}</Empty.Header
                  >{#if models.length === 0}<Empty.Content
                      ><Button onclick={() => manageResource('/models')}>{m.connect_go_models()}</Button></Empty.Content
                    >{/if}</Empty.Root>
              {/if}
            </section>
          </div>
        </Tabs.Content>

        <Tabs.Content value="code" class="mt-6 min-h-0 data-active:flex data-active:flex-col">
          <div
            class="grid gap-6 @min-[68rem]/connect:min-h-0 @min-[68rem]/connect:flex-1 @min-[68rem]/connect:grid-cols-[minmax(18rem,24rem)_minmax(0,1fr)] @min-[68rem]/connect:grid-rows-[minmax(0,1fr)]">
            <section
              class="min-w-0 @min-[68rem]/connect:min-h-0 @min-[68rem]/connect:overflow-y-auto"
              aria-labelledby="code-controls-title">
              <h2 id="code-controls-title" class="sr-only">{m.connect_api_example_setup()}</h2>
              <Field.FieldGroup>
                <ConnectControlRow
                  label={m.connect_api_format()}
                  labelFor="code-protocol"
                  hint={codeProtocols.find((protocol) => protocol.id === codeProtocol)?.path}>
                  <Select.Root type="single" bind:value={codeProtocol}>
                    <Select.Trigger id="code-protocol" class="w-full min-w-0"
                      ><span class="truncate"
                        >{codeProtocols.find((protocol) => protocol.id === codeProtocol)?.name}</span
                      ></Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each codeProtocols as protocol (protocol.id)}<Select.Item
                            value={protocol.id}
                            label={protocol.name}>{protocol.name}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                </ConnectControlRow>
                <ConnectControlRow label={m.common_model()} labelFor="code-model">
                  <SearchCombobox
                    id="code-model"
                    value={codeModelId}
                    options={models.map(logicalModelOption)}
                    placeholder={models.length ? m.common_select_model() : m.connect_add_model_first()}
                    emptyText={m.common_no_matching_models()}
                    disabled={models.length === 0}
                    onValueChange={(value: string) => (codeModelId = value)} />
                </ConnectControlRow>
                <ConnectControlRow label={m.common_api_key()} labelFor="code-key">
                  <Select.Root
                    type="single"
                    bind:value={
                      () => selectedCodeKey?.id ?? '',
                      (value: string) => {
                        codeKeyId = value
                      }
                    }
                    disabled={codeKeys.length === 0}>
                    <Select.Trigger id="code-key" class="w-full min-w-0"
                      ><span class="truncate"
                        >{selectedCodeKey
                          ? `${selectedCodeKey.name} · ${maskApiKey(selectedCodeKey.key)}`
                          : m.connect_select_api_key()}</span
                      ></Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each codeKeys as key (key.id)}<Select.Item value={key.id} label={key.name}
                            >{key.name} · {maskApiKey(key.key)}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                  {#snippet below()}
                    {#if !codeModel && models.length > 0}
                      <p class="text-sm text-muted-foreground">
                        {m.connect_choose_model_for_key()}
                      </p>
                    {/if}
                    {@render keyActions(selectedCodeKey !== undefined)}
                  {/snippet}
                </ConnectControlRow>
              </Field.FieldGroup>
            </section>

            <section class="flex min-h-0 min-w-0 flex-col" aria-labelledby="code-output-title">
              <div class="mb-3.5 flex shrink-0 flex-wrap items-start justify-between gap-4">
                <h2 id="code-output-title" class="route-section-title">
                  {m.connect_generated_request()}
                </h2>
                <Button onclick={copyGeneratedCode} disabled={!codeModel || !selectedCodeKey}
                  ><ClipboardCopyIcon data-icon="inline-start" />{m.common_copy()}</Button>
              </div>
              <Tabs.Root bind:value={codeLanguage} class="min-h-0 flex-1">
                <Tabs.List class="shrink-0" aria-label={m.connect_code_language()}>
                  {#each ['python', 'typescript', 'curl'] as language (language)}<Tabs.Trigger value={language}
                      >{language === 'typescript'
                        ? 'TypeScript'
                        : language === 'python'
                          ? 'Python'
                          : 'cURL'}</Tabs.Trigger
                    >{/each}
                </Tabs.List>
                <Tabs.Content value={codeLanguage} class="mt-3 min-h-0 data-active:flex data-active:flex-col">
                  {#if codeModel}
                    <pre class="route-code-plane min-h-96 flex-1">{generatedCode}</pre>
                    {#if !selectedCodeKey}
                      <Alert.Root class="mt-3 shrink-0" variant="warning" role="status">
                        {#if codeKeys.length === 0}
                          <Alert.Title>{m.connect_no_eligible_key()}</Alert.Title>
                          <Alert.Description
                            >{apiKeys.length
                              ? m.connect_no_eligible_key_description()
                              : m.connect_create_key_description()}</Alert.Description>
                        {:else}
                          <Alert.Description
                            >{m.connect_select_api_key_using_sample_current_output_contains()}</Alert.Description>
                        {/if}
                      </Alert.Root>
                    {/if}
                  {:else}
                    <Empty.Root class="min-h-72 border-y"
                      ><Empty.Header
                        >{#if models.length}
                          <Empty.Title>{m.common_select_model()}</Empty.Title>
                        {:else}
                          <Empty.Title>{m.connect_add_model_first()}</Empty.Title>
                          <Empty.Description
                            >{m.connect_connect_model_service_add_model_generating_code()}</Empty.Description>
                        {/if}</Empty.Header
                      >{#if models.length === 0}<Empty.Content
                          ><Button onclick={() => manageResource('/models')}>{m.connect_go_models()}</Button
                          ></Empty.Content
                        >{/if}</Empty.Root>
                  {/if}
                </Tabs.Content>
              </Tabs.Root>
            </section>
          </div>
        </Tabs.Content>

        <Tabs.Content value="mcp" class="mt-6 min-h-0 data-active:flex data-active:flex-col">
          {#if tab === 'mcp'}
            <div
              class="grid gap-6 @min-[68rem]/connect:min-h-0 @min-[68rem]/connect:flex-1 @min-[68rem]/connect:grid-cols-[minmax(18rem,24rem)_minmax(0,1fr)] @min-[68rem]/connect:grid-rows-[minmax(0,1fr)]">
              <section
                class="min-w-0 @min-[68rem]/connect:min-h-0 @min-[68rem]/connect:overflow-y-auto"
                aria-labelledby="mcp-controls-title">
                <h2 id="mcp-controls-title" class="sr-only">{m.connect_mcp_setup()}</h2>
                <Field.FieldGroup>
                  <ConnectControlRow label={m.common_api_key()} labelFor="mcp-key">
                    <Select.Root
                      type="single"
                      bind:value={
                        () => selectedMcpKey?.id ?? '',
                        (value: string) => {
                          mcpKeyId = value
                        }
                      }
                      disabled={mcpActiveKeys.length === 0}>
                      <Select.Trigger id="mcp-key" class="w-full min-w-0">
                        <span class="truncate">
                          {selectedMcpKey
                            ? `${selectedMcpKey.name} · ${maskApiKey(selectedMcpKey.key)}`
                            : m.connect_select_api_key()}
                        </span>
                      </Select.Trigger>
                      <Select.Content>
                        <Select.Group>
                          {#each mcpActiveKeys as key (key.id)}
                            <Select.Item value={key.id} label={key.name}>
                              {key.name} · {maskApiKey(key.key)}
                            </Select.Item>
                          {/each}
                        </Select.Group>
                      </Select.Content>
                    </Select.Root>
                    {#snippet below()}
                      {#if mcpActiveKeys.length === 0}
                        <p class="text-sm text-muted-foreground">{m.connect_mcp_no_active_keys()}</p>
                      {:else if !selectedMcpKey}
                        <p class="text-sm text-muted-foreground">{m.connect_mcp_select_key_hint()}</p>
                      {/if}
                      {@render keyActions(selectedMcpKey !== undefined)}
                    {/snippet}
                  </ConnectControlRow>
                </Field.FieldGroup>

                {#if !hasConnectMcpSlot && selectedMcpKey}
                  <div class="mt-4">
                    {#if selectedMcpKey.mcp_access_enabled}
                      <StatusIndicator tone="healthy" label={m.connect_mcp_auth_ok()} />
                    {:else}
                      <Alert.Root variant="warning" role="status">
                        <Alert.Title>{m.connect_mcp_auth_disabled_title()}</Alert.Title>
                        <Alert.Description class="flex flex-col items-start gap-2">
                          <p>{m.connect_mcp_auth_disabled_unified()}</p>
                          <Button
                            variant="outline"
                            size="sm"
                            href={resolve(`api-keys/${encodeURIComponent(selectedMcpKey.id)}`)}>
                            {m.connect_mcp_edit_key_access()}
                          </Button>
                        </Alert.Description>
                      </Alert.Root>
                    {/if}
                  </div>
                {/if}
              </section>

              {#if hasConnectMcpSlot}
                <section class="flex min-h-0 min-w-0 flex-col" aria-labelledby="mcp-output-title">
                  <ExtensionSlot
                    slot="connect:mcp-endpoints"
                    {capabilities}
                    props={{ host, apiKey: selectedMcpKey, capabilities }}
                    class="min-h-0 flex-1" />
                </section>
              {:else}
                <section class="flex min-h-0 min-w-0 flex-col gap-4" aria-labelledby="mcp-output-title">
                  <div class="flex flex-col gap-1 border-b pb-3">
                    <span class="text-xs font-medium text-muted-foreground">{m.connect_mcp_endpoint_url_label()}</span>
                    <TechnicalValue value={builtinMcpUrl} copyable />
                  </div>

                  <div class="flex shrink-0 flex-wrap items-center justify-between gap-4">
                    <h2 id="mcp-output-title" class="route-section-title font-structural">
                      {m.connect_mcp_config_heading()}
                    </h2>
                    <Button onclick={copyGeneratedMcpSnippet}>
                      <ClipboardCopyIcon data-icon="inline-start" />{mcpSnippetFormat === 'json'
                        ? m.connect_mcp_copy_json()
                        : m.connect_mcp_copy_curl()}
                    </Button>
                  </div>

                  <Tabs.Root bind:value={mcpSnippetFormat} class="min-h-0 flex-1">
                    <Tabs.List class="shrink-0" aria-label={m.connect_mcp_config_heading()}>
                      <Tabs.Trigger value="json">{m.connect_mcp_snippet_json_tab()}</Tabs.Trigger>
                      <Tabs.Trigger value="curl">{m.connect_mcp_snippet_curl_tab()}</Tabs.Trigger>
                    </Tabs.List>
                    <Tabs.Content value={mcpSnippetFormat} class="mt-3 min-h-0 data-active:flex data-active:flex-col">
                      <pre class="route-code-plane min-h-96 flex-1">{generatedMcpSnippet}</pre>
                    </Tabs.Content>
                  </Tabs.Root>
                </section>
              {/if}
            </div>
          {/if}
        </Tabs.Content>
      </Tabs.Root>
    {/if}
  </div>
</div>

<style>
/* 工作区（内容容器）宽度达到 68rem 才分栏：页面限定高度，预览填满余量，代码与过高的控件列分别滚动；单栏由主页面滚动。 */
.connect-page {
  min-height: calc(100svh - 5rem);
}
@container connect (min-width: 68rem) {
  .connect-page {
    height: calc(100svh - 5rem);
  }
  .route-code-plane {
    min-height: 0;
    max-height: none;
  }
}
@media (max-width: 767px) {
  .connect-page {
    min-height: calc(100svh - 4.5rem);
  }
}
</style>
