<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery } from '@tanstack/svelte-query'
import ClipboardCopyIcon from '@lucide/svelte/icons/clipboard-copy'
import CheckIcon from '@lucide/svelte/icons/check'
import Code2Icon from '@lucide/svelte/icons/code-2'
import ArrowRightIcon from '@lucide/svelte/icons/arrow-right'
import BoxIcon from '@lucide/svelte/icons/box'
import CircleCheckBigIcon from '@lucide/svelte/icons/circle-check-big'
import KeyRoundIcon from '@lucide/svelte/icons/key-round'
import PlugZapIcon from '@lucide/svelte/icons/plug-zap'
import TerminalSquareIcon from '@lucide/svelte/icons/terminal-square'
import { toast } from 'svelte-sonner'

import { admin, isTauri, proxyBase } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { applyConnectClient, asConnectClientApplyError, planConnectClient } from '$lib/connect-client-apply'
import { effectiveModelDisplayName, logicalModelSecondaryId, sortLogicalModels } from '$lib/logical-model'
import type { Route } from '$lib/types'
import {
  apiKeyAllowsModel,
  buildCode,
  CLI_TOOLS,
  defineClientModel,
  maskApiKey,
  protocolLabel,
  type ClaudeModelMappings,
  type CliToolId,
  type CodeLanguage,
  type ConnectClientApplyRequest,
  type GatewayProtocol,
} from '$lib/connect'
import PageHeader from '$lib/components/page-header.svelte'
import { Button } from '$lib/components/ui/button'
import * as Empty from '$lib/components/ui/empty'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'
import { Skeleton } from '$lib/components/ui/skeleton'
import * as Tabs from '$lib/components/ui/tabs'

const emptyKey = 'sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'
const modelsQuery = createQuery(() => ({ queryKey: ['models'], queryFn: admin.models.list }))
const keysQuery = createQuery(() => ({ queryKey: ['api-keys'], queryFn: admin.apiKeys.list }))
const proxyQuery = createQuery(() => ({
  queryKey: ['proxy-base'],
  queryFn: proxyBase,
  staleTime: Number.POSITIVE_INFINITY,
}))
let tab = $state<'cli' | 'code'>('cli')
let codeLanguage = $state<CodeLanguage>('python')
let codeProtocol = $state<GatewayProtocol>('openai-compatible')
let codeModelId = $state('')
let codeKeyId = $state('')
let cliToolId = $state<CliToolId>('codex-cli')
let cliKeyId = $state('')
let applyingClient = $state(false)
let claudeModelIds = $state<Record<keyof ClaudeModelMappings, string>>({
  defaultModel: '',
  haikuModel: '',
  sonnetModel: '',
  opusModel: '',
})

const models = $derived(sortLogicalModels(modelsQuery.data ?? []))
const apiKeys = $derived(keysQuery.data ?? [])
const host = $derived(proxyQuery.data ?? window.location.origin)
const resourcesPending = $derived(modelsQuery.isPending || keysQuery.isPending || proxyQuery.isPending)
const resourceError = $derived(modelsQuery.error ?? keysQuery.error ?? proxyQuery.error)
const blockingResourceError = $derived(
  (modelsQuery.data === undefined ? modelsQuery.error : null) ??
    (keysQuery.data === undefined ? keysQuery.error : null) ??
    (proxyQuery.data === undefined ? proxyQuery.error : null),
)
const codeModel = $derived(models.find((model) => model.id === codeModelId))
const codeKeys = $derived(codeModel ? apiKeys.filter((key) => apiKeyAllowsModel(key.model_ids, codeModel.id)) : [])
const selectedCodeKey = $derived(codeKeys.find((key) => key.id === codeKeyId))
const selectedCliKey = $derived(apiKeys.find((key) => key.id === cliKeyId))
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
      selectedCliKey.transparent_injection_enabled && selectedCliKey.inject_media_understanding,
  }
})
const connectPlanQuery = createQuery(() => ({
  queryKey: [
    'connect-client-plan',
    cliToolId,
    cliKeyId,
    host,
    clientConfigModels,
    claudeMappings,
    selectedCliKey?.transparent_injection_enabled,
    selectedCliKey?.inject_media_understanding,
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

const codeProtocols = [
  { id: 'openai-compatible', name: 'OpenAI Compatible', path: '/v1/chat/completions' },
  { id: 'open-responses', name: 'Open Responses', path: '/v1/responses' },
  { id: 'anthropic-messages', name: 'Anthropic Messages', path: '/v1/messages' },
  { id: 'google-gemini', name: 'Google Gemini', path: '/v1beta/models/{model}:generateContent' },
] as const

async function copyText(value: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(value)
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

function cliModelName(modelId: string): string | undefined {
  const model = cliModels.find((candidate) => candidate.id === modelId)
  return model ? effectiveModelDisplayName(model) : undefined
}
</script>

<svelte:head><title>{m.connect_connect_apps()} · Stravia</title></svelte:head>

{#snippet logicalModelOption(model: Route)}
  <span class="min-w-0 flex-1 truncate">{effectiveModelDisplayName(model)}</span>
  {#if logicalModelSecondaryId(model)}
    <span class="truncate font-technical text-xs text-muted-foreground">{model.model_id}</span>
  {/if}
{/snippet}

{#snippet logicalModelItems(options: Route[])}
  {#each options as model (model.id)}
    <Select.Item value={model.id} label={effectiveModelDisplayName(model)}>
      {@render logicalModelOption(model)}
    </Select.Item>
  {/each}
{/snippet}

<div class="route-page">
  <PageHeader
    eyebrow={m.common_setup()}
    title={m.connect_connect_apps()}
    description={m.connect_choose_client_or_code_copy_result()} />

  {#if resourcesPending}
    <div class="grid w-full max-w-72 grid-cols-2 gap-1" aria-label={m.connect_loading_output_modes()}>
      <Skeleton class="h-10" /><Skeleton class="h-10" />
    </div>
    <div class="mt-5 grid gap-6 min-[1100px]:grid-cols-12">
      <Skeleton class="h-96 min-[1100px]:col-span-5" />
      <Skeleton class="h-96 min-[1100px]:col-span-7" />
    </div>
  {:else if blockingResourceError}
    <section class="route-section" aria-labelledby="connect-resources-error">
      <h2 id="connect-resources-error" class="route-section-title">
        {m.connect_connection_setup_unavailable()}
      </h2>
      <p class="route-section-description text-destructive">
        {localizeBackendErrorMessage(blockingResourceError)}
      </p>
      <Button class="mt-3" variant="outline" onclick={retryResources}>{m.common_retry()}</Button>
    </section>
  {:else}
    {#if resourceError}
      <section class="route-section" role="alert">
        <p class="route-section-description text-destructive">
          {m.connect_stale_data_warning()}
        </p>
        <Button class="mt-3" variant="outline" onclick={retryResources}>{m.common_retry()}</Button>
      </section>
    {/if}

    {#if models.length === 0 || apiKeys.length === 0}
      <section class="connect-onboarding" aria-labelledby="connect-finish-setup-title">
        <div class="connect-route" aria-hidden="true">
          <div class:complete={models.length > 0} class="connect-route__node">
            {#if models.length > 0}
              <CircleCheckBigIcon />
            {:else}
              <BoxIcon />
            {/if}
          </div>
          <span class="connect-route__rail"></span>
          <div class:complete={apiKeys.length > 0} class="connect-route__node">
            {#if apiKeys.length > 0}
              <CircleCheckBigIcon />
            {:else}
              <KeyRoundIcon />
            {/if}
          </div>
          <span class="connect-route__rail"></span>
          <div class="connect-route__node connect-route__node--destination">
            <PlugZapIcon />
          </div>
        </div>

        <div class="connect-onboarding__copy">
          <p class="connect-onboarding__eyebrow">{m.connect_setup_format()}</p>
          <h2 id="connect-finish-setup-title">{m.connect_finish_setup()}</h2>
          <p>{m.connect_finish_setup_description()}</p>
        </div>

        <ul class="connect-actions">
          {#if models.length === 0}
            <li>
              <a class="connect-action" href={resolve('/models')}>
                <span class="connect-action__icon"><BoxIcon /></span>
                <span class="connect-action__copy">
                  <span class="connect-action__label">{m.common_model()}</span>
                  <strong>{m.connect_add_a_model()}</strong>
                </span>
                <ArrowRightIcon class="connect-action__arrow" />
              </a>
            </li>
          {/if}
          {#if apiKeys.length === 0}
            <li>
              <a class="connect-action" href={resolve('/api-keys')}>
                <span class="connect-action__icon"><KeyRoundIcon /></span>
                <span class="connect-action__copy">
                  <span class="connect-action__label">{m.common_api_key()}</span>
                  <strong>{m.connect_create_an_api_key()}</strong>
                </span>
                <ArrowRightIcon class="connect-action__arrow" />
              </a>
            </li>
          {/if}
        </ul>
      </section>
    {:else}
      <Tabs.Root bind:value={tab}>
        <Tabs.List class="grid w-full max-w-72 grid-cols-2" aria-label={m.connect_setup_format()}>
          <Tabs.Trigger value="cli"
            ><TerminalSquareIcon data-icon="inline-start" />{m.connect_clients_label()}</Tabs.Trigger>
          <Tabs.Trigger value="code"><Code2Icon data-icon="inline-start" />{m.connect_code()}</Tabs.Trigger>
        </Tabs.List>

        <Tabs.Content value="cli" class="mt-5">
          <div class="grid gap-6 min-[1100px]:grid-cols-12">
            <section class="route-section min-[1100px]:col-span-5" aria-labelledby="cli-controls-title">
              <div class="route-section-header">
                <div>
                  <h2 id="cli-controls-title" class="route-section-title">
                    {m.connect_client_setup()}
                  </h2>
                  <p class="route-section-description">
                    {selectedTool.description()}
                  </p>
                </div>
              </div>
              <Field.FieldGroup>
                <Field.Field size="select">
                  <Field.FieldLabel for="cli-tool">{m.connect_client()}</Field.FieldLabel>
                  <Select.Root type="single" bind:value={cliToolId}>
                    <Select.Trigger id="cli-tool" class="w-full">{selectedTool.name}</Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each CLI_TOOLS as tool (tool.id)}<Select.Item value={tool.id} label={tool.name}
                            >{tool.name}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                </Field.Field>
                <Field.Field size="select">
                  <Field.FieldLabel for="cli-key">{m.common_api_key()}</Field.FieldLabel>
                  <Select.Root type="single" bind:value={cliKeyId} disabled={apiKeys.length === 0}>
                    <Select.Trigger id="cli-key" class="w-full"
                      >{selectedCliKey
                        ? `${selectedCliKey.name} · ${maskApiKey(selectedCliKey.key)}`
                        : m.connect_select_api_key()}</Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each apiKeys as key (key.id)}<Select.Item value={key.id} label={key.name}
                            >{key.name} · {maskApiKey(key.key)}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                  {#if !selectedCliKey}
                    <Field.FieldDescription>{m.connect_select_api_key_client_description()}</Field.FieldDescription>
                  {:else if cliModels.length === 0}
                    <Field.FieldDescription class="text-warning"
                      >{m.connect_api_key_has_no_models_description()}</Field.FieldDescription>
                  {:else if cliToolId === 'claude-code'}
                    <Field.FieldDescription>{m.connect_model_mapping_scope()}</Field.FieldDescription>
                  {:else}
                    <Field.FieldDescription>{m.connect_all_api_key_models_included()}</Field.FieldDescription>
                  {/if}
                </Field.Field>
                {#if selectedCliKey && cliModels.length > 0}
                  {#if cliToolId === 'claude-code'}
                    <Field.Field size="select">
                      <Field.FieldLabel for="cli-default-model">{m.connect_default_model()}</Field.FieldLabel>
                      <Select.Root type="single" bind:value={claudeModelIds.defaultModel}>
                        <Select.Trigger id="cli-default-model" class="w-full"
                          >{cliModelName(claudeModelIds.defaultModel) ?? m.common_select_model()}</Select.Trigger>
                        <Select.Content
                          ><Select.Group>{@render logicalModelItems(cliModels)}</Select.Group></Select.Content>
                      </Select.Root>
                    </Field.Field>
                    <Field.Field size="select">
                      <Field.FieldLabel for="cli-haiku-model">{m.connect_haiku_model_mapping()}</Field.FieldLabel>
                      <Select.Root type="single" bind:value={claudeModelIds.haikuModel}>
                        <Select.Trigger id="cli-haiku-model" class="w-full"
                          >{cliModelName(claudeModelIds.haikuModel) ?? m.common_select_model()}</Select.Trigger>
                        <Select.Content
                          ><Select.Group>{@render logicalModelItems(cliModels)}</Select.Group></Select.Content>
                      </Select.Root>
                    </Field.Field>
                    <Field.Field size="select">
                      <Field.FieldLabel for="cli-sonnet-model">{m.connect_sonnet_model_mapping()}</Field.FieldLabel>
                      <Select.Root type="single" bind:value={claudeModelIds.sonnetModel}>
                        <Select.Trigger id="cli-sonnet-model" class="w-full"
                          >{cliModelName(claudeModelIds.sonnetModel) ?? m.common_select_model()}</Select.Trigger>
                        <Select.Content
                          ><Select.Group>{@render logicalModelItems(cliModels)}</Select.Group></Select.Content>
                      </Select.Root>
                    </Field.Field>
                    <Field.Field size="select">
                      <Field.FieldLabel for="cli-opus-model">{m.connect_opus_model_mapping()}</Field.FieldLabel>
                      <Select.Root type="single" bind:value={claudeModelIds.opusModel}>
                        <Select.Trigger id="cli-opus-model" class="w-full"
                          >{cliModelName(claudeModelIds.opusModel) ?? m.common_select_model()}</Select.Trigger>
                        <Select.Content
                          ><Select.Group>{@render logicalModelItems(cliModels)}</Select.Group></Select.Content>
                      </Select.Root>
                    </Field.Field>
                  {/if}
                {/if}
              </Field.FieldGroup>
            </section>

            <section class="route-section min-[1100px]:col-span-7" aria-labelledby="cli-output-title">
              <div class="route-section-header">
                <div>
                  <h2 id="cli-output-title" class="route-section-title">{selectedTool.name}</h2>
                  <p class="route-section-description">
                    {protocolLabel(selectedTool.protocol)} · <span class="font-technical">{host}</span>
                  </p>
                  {#if isTauri && connectPlanQuery.data}
                    <div class="mt-2 space-y-1">
                      <p class="text-xs font-medium text-foreground">{m.connect_global_config_paths()}</p>
                      {#each connectPlanQuery.data.paths as path (path)}
                        <p class="font-technical break-all text-xs text-muted-foreground">{path}</p>
                      {/each}
                    </div>
                  {/if}
                </div>
                <div class="flex flex-wrap gap-2">
                  {#if isTauri}
                    <Button
                      onclick={() => void applySelectedClient()}
                      disabled={!connectClientInput || !connectPlanQuery.data || applyingClient}
                      ><CheckIcon data-icon="inline-start" />{applyingClient
                        ? m.connect_applying()
                        : m.connect_apply()}</Button>
                  {/if}
                  <Button
                    variant="outline"
                    onclick={() => void copyText(generatedCliConfig)}
                    disabled={!generatedCliConfig}
                    ><ClipboardCopyIcon data-icon="inline-start" />{m.common_copy()}</Button>
                </div>
              </div>
              {#if connectPlanQuery.isError}
                {@const planError = asConnectClientApplyError(connectPlanQuery.error)}
                <div class="mb-3 border-l-2 border-destructive bg-destructive/5 px-3 py-2" role="alert">
                  <p class="text-sm font-medium text-destructive">{m.connect_apply_plan_failed()}</p>
                  {#if planError.path}
                    <p class="font-technical mt-1 break-all text-xs text-muted-foreground">{planError.path}</p>
                  {/if}
                  <p class="mt-1 text-sm text-muted-foreground">{planError.message}</p>
                </div>
              {/if}
              {#if generatedCliConfig}
                <pre class="route-code-plane">{generatedCliConfig}</pre>
              {:else}
                <Empty.Root class="min-h-72 border-y"
                  ><Empty.Header
                    >{#if !selectedCliKey}
                      <Empty.Title>{m.connect_select_api_key()}</Empty.Title>
                      <Empty.Description>{m.connect_select_api_key_client_description()}</Empty.Description>
                    {:else if cliModels.length === 0}
                      <Empty.Title>{m.connect_api_key_has_no_models()}</Empty.Title>
                      <Empty.Description>{m.connect_api_key_has_no_models_description()}</Empty.Description>
                    {:else if cliToolId === 'claude-code'}
                      <Empty.Title>{m.connect_complete_model_mappings()}</Empty.Title>
                      <Empty.Description>{m.connect_complete_model_mappings_description()}</Empty.Description>
                    {:else if connectPlanQuery.isPending}
                      <Empty.Title>{m.connect_loading_global_config()}</Empty.Title>
                      <Empty.Description>{m.connect_loading_global_config_description()}</Empty.Description>
                    {:else}
                      <Empty.Title>{m.connect_apply_plan_failed()}</Empty.Title>
                      <Empty.Description>{m.connect_fix_global_config_retry()}</Empty.Description>
                    {/if}</Empty.Header
                  >{#if selectedCliKey && cliModels.length === 0}<Empty.Content
                      ><Button href="/api-keys">{m.connect_go_api_keys()}</Button></Empty.Content
                    >{/if}</Empty.Root>
              {/if}
            </section>
          </div>
        </Tabs.Content>

        <Tabs.Content value="code" class="mt-5">
          <div class="grid gap-6 min-[1100px]:grid-cols-12">
            <section class="route-section min-[1100px]:col-span-5" aria-labelledby="code-controls-title">
              <div class="route-section-header">
                <div>
                  <h2 id="code-controls-title" class="route-section-title">
                    {m.connect_api_example_setup()}
                  </h2>
                  <p class="route-section-description">
                    {m.connect_choose_api_format_model_key_example()}
                  </p>
                </div>
              </div>
              <Field.FieldGroup>
                <Field.Field size="select">
                  <Field.FieldLabel for="code-protocol">{m.connect_api_format()}</Field.FieldLabel>
                  <Select.Root type="single" bind:value={codeProtocol}>
                    <Select.Trigger id="code-protocol" class="w-full"
                      >{codeProtocols.find((protocol) => protocol.id === codeProtocol)?.name}</Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each codeProtocols as protocol (protocol.id)}<Select.Item
                            value={protocol.id}
                            label={protocol.name}>{protocol.name}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                  <Field.FieldDescription class="font-technical break-all"
                    >{codeProtocols.find((protocol) => protocol.id === codeProtocol)?.path}</Field.FieldDescription>
                </Field.Field>
                <Field.Field size="select">
                  <Field.FieldLabel for="code-model">{m.common_model()}</Field.FieldLabel>
                  <Select.Root type="single" bind:value={codeModelId} disabled={models.length === 0}>
                    <Select.Trigger id="code-model" class="w-full"
                      >{(codeModel ? effectiveModelDisplayName(codeModel) : undefined) ??
                        (models.length ? m.common_select_model() : m.connect_add_model_first())}</Select.Trigger>
                    <Select.Content><Select.Group>{@render logicalModelItems(models)}</Select.Group></Select.Content>
                  </Select.Root>
                </Field.Field>
                <Field.Field size="select">
                  <Field.FieldLabel for="code-key">{m.common_api_key()}</Field.FieldLabel>
                  <Select.Root type="single" bind:value={codeKeyId} disabled={codeKeys.length === 0}>
                    <Select.Trigger id="code-key" class="w-full"
                      >{selectedCodeKey
                        ? `${selectedCodeKey.name} · ${maskApiKey(selectedCodeKey.key)}`
                        : m.connect_select_api_key()}</Select.Trigger>
                    <Select.Content
                      ><Select.Group
                        >{#each codeKeys as key (key.id)}<Select.Item value={key.id} label={key.name}
                            >{key.name} · {maskApiKey(key.key)}</Select.Item
                          >{/each}</Select.Group
                      ></Select.Content>
                  </Select.Root>
                </Field.Field>
              </Field.FieldGroup>
            </section>

            <section class="route-section min-[1100px]:col-span-7" aria-labelledby="code-output-title">
              <div class="route-section-header">
                <div>
                  <h2 id="code-output-title" class="route-section-title">
                    {m.connect_generated_request()}
                  </h2>
                  <p class="route-section-description">
                    {codeProtocols.find((protocol) => protocol.id === codeProtocol)?.name}
                  </p>
                </div>
                <Button variant="outline" onclick={() => void copyText(generatedCode)} disabled={!codeModel}
                  ><ClipboardCopyIcon data-icon="inline-start" />{m.common_copy()}</Button>
              </div>
              <Tabs.Root bind:value={codeLanguage}>
                <Tabs.List class="grid w-full max-w-72 grid-cols-3" aria-label={m.connect_code_language()}>
                  {#each ['python', 'typescript', 'curl'] as language (language)}<Tabs.Trigger value={language}
                      >{language === 'typescript'
                        ? 'TypeScript'
                        : language === 'python'
                          ? 'Python'
                          : 'cURL'}</Tabs.Trigger
                    >{/each}
                </Tabs.List>
                <Tabs.Content value={codeLanguage} class="mt-3">
                  {#if codeModel}
                    <pre class="route-code-plane">{generatedCode}</pre>
                    {#if !selectedCodeKey}<p class="mt-3 text-sm text-warning">
                        {m.connect_select_api_key_using_sample_current_output_contains()}
                      </p>{/if}
                  {:else}
                    <Empty.Root class="min-h-72 border-y"
                      ><Empty.Header
                        ><Empty.Title>{m.common_select_model()}</Empty.Title><Empty.Description
                          >{models.length
                            ? m.connect_generated_request_appear_here()
                            : m.connect_connect_model_service_add_model_generating_code()}</Empty.Description
                        ></Empty.Header
                      >{#if models.length === 0}<Empty.Content
                          ><Button href="/models">{m.connect_go_models()}</Button></Empty.Content
                        >{/if}</Empty.Root>
                  {/if}
                </Tabs.Content>
              </Tabs.Root>
            </section>
          </div>
        </Tabs.Content>
      </Tabs.Root>
    {/if}
  {/if}
</div>

<style>
.connect-onboarding {
  position: relative;
  isolation: isolate;
  overflow: hidden;
  border-radius: 1rem;
  background:
    radial-gradient(circle at 82% 8%, color-mix(in oklch, var(--primary) 13%, transparent) 0, transparent 35%),
    var(--card);
  padding: clamp(1.5rem, 4vw, 2.75rem);
  box-shadow:
    inset 0 0 0 1px color-mix(in oklch, var(--border) 82%, transparent),
    0 1px 2px rgb(17 20 23 / 0.05),
    0 18px 42px rgb(17 20 23 / 0.07);
}

.connect-onboarding::after {
  position: absolute;
  z-index: -1;
  width: 13rem;
  height: 13rem;
  border: 1px solid color-mix(in oklch, var(--primary) 12%, transparent);
  border-radius: 50%;
  content: '';
  inset: -8rem -4rem auto auto;
}

.connect-route {
  display: grid;
  max-width: 27rem;
  grid-template-columns: 2.75rem minmax(2rem, 1fr) 2.75rem minmax(2rem, 1fr) 2.75rem;
  align-items: center;
  margin-bottom: 2rem;
}

.connect-route__node {
  display: grid;
  width: 2.75rem;
  height: 2.75rem;
  place-items: center;
  border-radius: 0.75rem;
  background: var(--background);
  color: var(--muted-foreground);
  box-shadow:
    inset 0 0 0 1px color-mix(in oklch, var(--border) 86%, transparent),
    0 2px 5px rgb(17 20 23 / 0.06);
}

.connect-route__node :global(svg) {
  width: 1.05rem;
  height: 1.05rem;
}

.connect-route__node.complete {
  background: color-mix(in oklch, var(--success) 10%, var(--card));
  color: var(--success);
}

.connect-route__node--destination {
  background: var(--primary);
  color: var(--primary-foreground);
  box-shadow:
    0 0 0 5px color-mix(in oklch, var(--primary) 10%, transparent),
    0 5px 12px color-mix(in oklch, var(--primary) 24%, transparent);
}

.connect-route__rail {
  height: 1px;
  background: linear-gradient(
    90deg,
    color-mix(in oklch, var(--primary) 22%, var(--border)),
    color-mix(in oklch, var(--primary) 62%, var(--border))
  );
}

.connect-onboarding__copy {
  max-width: 36rem;
}

.connect-onboarding__eyebrow {
  margin-bottom: 0.5rem;
  color: var(--primary);
  font-family: var(--font-structural);
  font-size: 0.6875rem;
  font-weight: 600;
  letter-spacing: 0.14em;
  text-transform: uppercase;
}

.connect-onboarding__copy h2 {
  font-family: var(--font-structural);
  font-size: clamp(1.35rem, 3vw, 1.75rem);
  line-height: 1.2;
  font-weight: 600;
  letter-spacing: -0.02em;
  text-wrap: balance;
}

.connect-onboarding__copy > p:last-child {
  margin-top: 0.625rem;
  color: var(--muted-foreground);
  font-size: 0.875rem;
  line-height: 1.55;
  text-wrap: pretty;
}

.connect-actions {
  display: grid;
  max-width: 46rem;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0.75rem;
  margin-top: 1.75rem;
}

.connect-actions:has(> li:only-child) {
  grid-template-columns: minmax(0, 22.5rem);
}

.connect-action {
  display: grid;
  min-height: 4.75rem;
  grid-template-columns: 2.5rem minmax(0, 1fr) 1rem;
  align-items: center;
  gap: 0.875rem;
  border-radius: 0.875rem;
  background: color-mix(in oklch, var(--background) 76%, var(--card));
  padding: 0.75rem;
  color: var(--foreground);
  box-shadow: inset 0 0 0 1px color-mix(in oklch, var(--border) 88%, transparent);
  transition:
    background-color 160ms cubic-bezier(0.2, 0, 0, 1),
    box-shadow 160ms cubic-bezier(0.2, 0, 0, 1),
    transform 160ms cubic-bezier(0.2, 0, 0, 1);
}

.connect-action:hover {
  background: var(--card);
  box-shadow:
    inset 0 0 0 1px color-mix(in oklch, var(--primary) 38%, var(--border)),
    0 8px 20px rgb(17 20 23 / 0.08);
  transform: translateY(-2px);
}

.connect-action:active {
  transform: scale(0.96);
}

.connect-action:focus-visible {
  outline: 3px solid color-mix(in oklch, var(--ring) 48%, transparent);
  outline-offset: 2px;
}

.connect-action__icon {
  display: grid;
  width: 2.5rem;
  height: 2.5rem;
  place-items: center;
  border-radius: 0.625rem;
  background: color-mix(in oklch, var(--primary) 10%, var(--card));
  color: var(--primary);
}

.connect-action__icon :global(svg) {
  width: 1rem;
  height: 1rem;
}

.connect-action__copy {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 0.15rem;
}

.connect-action__label {
  color: var(--muted-foreground);
  font-size: 0.6875rem;
  font-weight: 500;
  letter-spacing: 0.06em;
  text-transform: uppercase;
}

.connect-action__copy strong {
  overflow: hidden;
  font-size: 0.875rem;
  font-weight: 600;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.connect-action :global(.connect-action__arrow) {
  width: 1rem;
  height: 1rem;
  color: var(--muted-foreground);
  transition:
    color 160ms cubic-bezier(0.2, 0, 0, 1),
    transform 160ms cubic-bezier(0.2, 0, 0, 1);
}

.connect-action:hover :global(.connect-action__arrow) {
  color: var(--primary);
  transform: translateX(2px);
}

@media (max-width: 600px) {
  .connect-onboarding {
    padding: 1.25rem;
  }

  .connect-route {
    margin-bottom: 1.5rem;
  }

  .connect-actions {
    grid-template-columns: 1fr;
  }

  .connect-actions:has(> li:only-child) {
    grid-template-columns: 1fr;
  }
}

@media (prefers-reduced-motion: reduce) {
  .connect-action,
  .connect-action :global(.connect-action__arrow) {
    transition-duration: 0.01ms;
  }
}
</style>
