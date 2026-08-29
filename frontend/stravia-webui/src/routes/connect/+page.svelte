<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { createQuery } from '@tanstack/svelte-query'
import ClipboardCopyIcon from '@lucide/svelte/icons/clipboard-copy'
import Code2Icon from '@lucide/svelte/icons/code-2'
import TerminalSquareIcon from '@lucide/svelte/icons/terminal-square'
import { toast } from 'svelte-sonner'

import { admin, proxyBase } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import {
  buildCliConfig,
  buildCode,
  CLI_TOOLS,
  defineClientModel,
  maskApiKey,
  protocolLabel,
  type ClaudeModelMappings,
  type CliToolId,
  type CodeLanguage,
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
let cliToolId = $state<CliToolId>('claude-code')
let cliKeyId = $state('')
let cliDefaultModelId = $state('')
let claudeModelIds = $state<Record<keyof ClaudeModelMappings, string>>({
  defaultModel: '',
  haikuModel: '',
  sonnetModel: '',
  opusModel: '',
})

const models = $derived(modelsQuery.data ?? [])
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
const codeKeys = $derived(codeModel ? apiKeys.filter((key) => key.model_ids.includes(codeModel.id)) : [])
const selectedCodeKey = $derived(codeKeys.find((key) => key.id === codeKeyId))
const selectedCliKey = $derived(apiKeys.find((key) => key.id === cliKeyId))
const cliModels = $derived(
  selectedCliKey ? models.filter((model) => selectedCliKey.model_ids.includes(model.id)) : [],
)
const selectedCliDefaultModel = $derived(cliModels.find((model) => model.id === cliDefaultModelId))
const selectedTool = $derived(CLI_TOOLS.find((tool) => tool.id === cliToolId) ?? CLI_TOOLS[0])
const codeApiKey = $derived(selectedCodeKey?.key ?? emptyKey)
const clientConfigModels = $derived(cliModels.map(defineClientModel))
const claudeMappings = $derived.by((): ClaudeModelMappings | undefined => {
  const mappings = {
    defaultModel: cliModels.find((model) => model.id === claudeModelIds.defaultModel)?.name,
    haikuModel: cliModels.find((model) => model.id === claudeModelIds.haikuModel)?.name,
    sonnetModel: cliModels.find((model) => model.id === claudeModelIds.sonnetModel)?.name,
    opusModel: cliModels.find((model) => model.id === claudeModelIds.opusModel)?.name,
  }
  if (!mappings.defaultModel || !mappings.haikuModel || !mappings.sonnetModel || !mappings.opusModel)
    return undefined
  return mappings as ClaudeModelMappings
})
const generatedCode = $derived(
  buildCode({
    protocol: codeProtocol,
    model: codeModel?.name ?? 'gpt-4o',
    apiKey: codeApiKey,
    host,
    language: codeLanguage,
  }),
)
const generatedCliConfig = $derived.by(() => {
  if (!selectedCliKey || cliModels.length === 0) return ''
  if (cliToolId === 'claude-code') {
    if (!claudeMappings) return ''
    return buildCliConfig({
      tool: cliToolId,
      host,
      apiKey: selectedCliKey.key,
      models: clientConfigModels,
      mappings: claudeMappings,
    })
  }
  if (!selectedCliDefaultModel) return ''
  return buildCliConfig({
    tool: cliToolId,
    host,
    apiKey: selectedCliKey.key,
    models: clientConfigModels,
    defaultModel: selectedCliDefaultModel.name,
  })
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

function retryResources(): void {
  void Promise.all([modelsQuery.refetch(), keysQuery.refetch(), proxyQuery.refetch()])
}

function cliModelName(modelId: string): string | undefined {
  return cliModels.find((model) => model.id === modelId)?.name
}
</script>

<svelte:head><title>{m.connect_connect_apps()} · Stravia</title></svelte:head>

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
      <section class="route-section" aria-labelledby="connect-finish-setup-title">
        <div class="route-section-header">
          <div>
            <h2 id="connect-finish-setup-title" class="route-section-title">{m.connect_finish_setup()}</h2>
          </div>
        </div>
        <ul class="flex flex-col gap-2 border-y py-4">
          {#if models.length === 0}
            <li>
              <a class="font-medium text-primary underline-offset-4 hover:underline" href={resolve('/models')}>
                {m.connect_add_a_model()}
              </a>
            </li>
          {/if}
          {#if apiKeys.length === 0}
            <li>
              <a class="font-medium text-primary underline-offset-4 hover:underline" href={resolve('/api-keys')}>
                {m.connect_create_an_api_key()}
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
                        ><Select.Group
                          >{#each cliModels as model (model.id)}<Select.Item value={model.id} label={model.name}
                              >{model.name}</Select.Item
                            >{/each}</Select.Group
                        ></Select.Content>
                    </Select.Root>
                  </Field.Field>
                  <Field.Field size="select">
                    <Field.FieldLabel for="cli-haiku-model">{m.connect_haiku_model_mapping()}</Field.FieldLabel>
                    <Select.Root type="single" bind:value={claudeModelIds.haikuModel}>
                      <Select.Trigger id="cli-haiku-model" class="w-full"
                        >{cliModelName(claudeModelIds.haikuModel) ?? m.common_select_model()}</Select.Trigger>
                      <Select.Content
                        ><Select.Group
                          >{#each cliModels as model (model.id)}<Select.Item value={model.id} label={model.name}
                              >{model.name}</Select.Item
                            >{/each}</Select.Group
                        ></Select.Content>
                    </Select.Root>
                  </Field.Field>
                  <Field.Field size="select">
                    <Field.FieldLabel for="cli-sonnet-model">{m.connect_sonnet_model_mapping()}</Field.FieldLabel>
                    <Select.Root type="single" bind:value={claudeModelIds.sonnetModel}>
                      <Select.Trigger id="cli-sonnet-model" class="w-full"
                        >{cliModelName(claudeModelIds.sonnetModel) ?? m.common_select_model()}</Select.Trigger>
                      <Select.Content
                        ><Select.Group
                          >{#each cliModels as model (model.id)}<Select.Item value={model.id} label={model.name}
                              >{model.name}</Select.Item
                            >{/each}</Select.Group
                        ></Select.Content>
                    </Select.Root>
                  </Field.Field>
                  <Field.Field size="select">
                    <Field.FieldLabel for="cli-opus-model">{m.connect_opus_model_mapping()}</Field.FieldLabel>
                    <Select.Root type="single" bind:value={claudeModelIds.opusModel}>
                      <Select.Trigger id="cli-opus-model" class="w-full"
                        >{cliModelName(claudeModelIds.opusModel) ?? m.common_select_model()}</Select.Trigger>
                      <Select.Content
                        ><Select.Group
                          >{#each cliModels as model (model.id)}<Select.Item value={model.id} label={model.name}
                              >{model.name}</Select.Item
                            >{/each}</Select.Group
                        ></Select.Content>
                    </Select.Root>
                  </Field.Field>
                {:else}
                  <Field.Field size="select">
                    <Field.FieldLabel for="cli-default-model">{m.connect_default_model()}</Field.FieldLabel>
                    <Select.Root type="single" bind:value={cliDefaultModelId}>
                      <Select.Trigger id="cli-default-model" class="w-full"
                        >{selectedCliDefaultModel?.name ?? m.connect_select_default_model()}</Select.Trigger>
                      <Select.Content
                        ><Select.Group
                          >{#each cliModels as model (model.id)}<Select.Item value={model.id} label={model.name}
                              >{model.name}</Select.Item
                            >{/each}</Select.Group
                        ></Select.Content>
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
              </div>
              <Button
                variant="outline"
                onclick={() => void copyText(generatedCliConfig)}
                disabled={!generatedCliConfig}
                ><ClipboardCopyIcon data-icon="inline-start" />{m.common_copy()}</Button>
            </div>
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
                  {:else}
                    <Empty.Title>{m.connect_select_default_model()}</Empty.Title>
                    <Empty.Description>{m.connect_select_default_model_description()}</Empty.Description>
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
                    >{codeModel?.name ??
                      (models.length ? m.common_select_model() : m.connect_add_model_first())}</Select.Trigger>
                  <Select.Content
                    ><Select.Group
                      >{#each models as model (model.id)}<Select.Item value={model.id} label={model.name}
                          >{model.name}</Select.Item
                        >{/each}</Select.Group
                    ></Select.Content>
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
              <Tabs.List
                class="grid w-full max-w-72 grid-cols-3"
                aria-label={m.connect_code_language()}>
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
