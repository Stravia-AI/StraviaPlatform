<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SearchIcon from '@lucide/svelte/icons/search'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { localeState } from '$lib/localization.svelte'
import {
  buildProviderOptions,
  defaultProviderName,
  optionDescription,
  optionLabel,
  providerNameAfterOptionChange,
  type ProviderOption,
} from '$lib/provider-options'
import type { CreateProvider, OAuthCandidateConfiguration, Provider, ProviderConfigurationPreview } from '$lib/types'
import ProviderConfigFields from '$lib/components/provider-config-fields.svelte'
import ProviderOAuthAuthorization from '$lib/components/provider-oauth-authorization.svelte'
import * as Alert from '$lib/components/ui/alert'
import { Button, buttonVariants } from '$lib/components/ui/button'
import ProviderMark from '$lib/components/provider-mark.svelte'
import * as Field from '$lib/components/ui/field'
import * as Empty from '$lib/components/ui/empty'
import * as InputGroup from '$lib/components/ui/input-group'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import * as Tabs from '$lib/components/ui/tabs'

interface ProviderForm {
  name: string
  baseUrl: string
  protocol: string
  useProxy: boolean
  values: Record<string, unknown>
}

interface Props {
  open?: boolean
  onSaved?: (provider: Provider) => void
}

let { open = $bindable(false), onSaved }: Props = $props()
let step = $state<'select' | 'configure'>('select')
let search = $state('')
let focusedOptionKey = $state('')
let selectedOptionKey = $state('')
let form = $state<ProviderForm>({ name: '', baseUrl: '', protocol: '', useProxy: false, values: {} })
let oauthSessionId = $state<string>()
let oauthReady = $state(false)
let oauthAuthorization = $state<{
  cancel: () => Promise<void>
  consume: () => void
  updateProxy: (useProxy: boolean) => Promise<void>
}>()
let saving = $state(false)
let reviewing = $state(false)
let reviewRequestId = 0
let refreshingServices = $state(false)
let preview = $state<ProviderConfigurationPreview>()
let previewFailure = $state('')

const queryClient = useQueryClient()
const providerDescriptorsQuery = createQuery(() => ({
  queryKey: ['provider-descriptors'],
  queryFn: admin.providers.descriptors,
}))
const options = $derived(buildProviderOptions(providerDescriptorsQuery.data ?? []))
const selectedOption = $derived(options.find((option) => option.key === selectedOptionKey))
const configFields = $derived(selectedOption?.descriptor.config_fields ?? [])
const oauthSessionSecretFields = $derived(
  oauthReady ? configFields.filter((field) => field.secret && field.required).map((field) => field.key) : [],
)
const supportsConfigValidation = $derived(selectedOption?.channel.capabilities.includes('config_validation') ?? false)
const previewIssues = $derived(preview?.issues ?? [])
const globalIssues = $derived(previewIssues.filter((issue) => !issue.field))
const previewAccepted = $derived(Boolean(preview) && previewIssues.length === 0)
const oauthConfiguration = $derived.by((): OAuthCandidateConfiguration => ({
  base_url: form.baseUrl.trim(),
  protocol: form.protocol || selectedOption?.channel.protocol || undefined,
  options: configurationValues(false),
  credentials: configurationValues(true),
}))
const oauthProvider = $derived(Boolean(selectedOption?.channel.auth))
const providerOptions = $derived.by(() => {
  const query = search.trim().toLocaleLowerCase(localeState.current)
  return options.filter((option) => {
    if (!query) return true
    const auth = option.channel.auth ? 'oauth account 账号' : 'api key'
    const text = `${option.descriptor.provider_id} ${option.descriptor.catalog_id ?? ''} ${option.descriptor.display_name} ${option.channel.id} ${option.channel.name} ${optionDescription(option, localeState.current)} ${auth}`
    return text.toLocaleLowerCase(localeState.current).includes(query)
  })
})

function handleOpenChange(nextOpen: boolean): void {
  open = nextOpen
  if (!nextOpen) void oauthAuthorization?.cancel()
}

function invalidatePreview(): void {
  reviewRequestId += 1
  reviewing = false
  preview = undefined
  previewFailure = ''
}

function configurationChanged(): void {
  invalidatePreview()
  if (!oauthSessionId) return
  oauthSessionId = undefined
  oauthReady = false
  void oauthAuthorization?.cancel()
}

async function chooseOption(option: ProviderOption): Promise<void> {
  await oauthAuthorization?.cancel()
  const name = providerNameAfterOptionChange(form.name, selectedOption, option)
  const values: Record<string, unknown> = {}
  for (const field of option.descriptor.config_fields) {
    if (!field.secret && field.default_json != null) values[field.key] = field.default_json
    else if (!field.secret && field.required && field.kind.type === 'bool') values[field.key] = false
  }
  selectedOptionKey = option.key
  form = {
    name,
    baseUrl: option.channel.default_base_url ?? '',
    protocol: option.channel.protocol ?? option.channel.protocols?.[0]?.value ?? '',
    useProxy: form.useProxy,
    values,
  }
  oauthSessionId = undefined
  oauthReady = false
  invalidatePreview()
  step = 'configure'
}

async function goBack(): Promise<void> {
  await oauthAuthorization?.cancel()
  step = 'select'
}

function changeStep(value: string): void {
  if (value === 'select') void goBack()
  else if (selectedOption) step = 'configure'
}

async function refreshServices(): Promise<void> {
  refreshingServices = true
  try {
    await providerDescriptorsQuery.refetch()
  } finally {
    refreshingServices = false
  }
}

function handleProviderOptionKeydown(event: KeyboardEvent): void {
  if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) return
  const current = event.currentTarget as HTMLButtonElement
  const grid = current.closest<HTMLElement>('[data-provider-grid]')
  if (!grid) return
  const cards = Array.from(grid.querySelectorAll<HTMLButtonElement>('[data-provider-option]')).filter(
    (card) => card.offsetParent !== null,
  )
  const currentIndex = cards.indexOf(current)
  if (currentIndex < 0 || cards.length < 2) return
  const firstTop = cards[0].getBoundingClientRect().top
  const nextRowIndex = cards.findIndex((card) => card.getBoundingClientRect().top > firstTop + 1)
  const columns = nextRowIndex < 0 ? cards.length : nextRowIndex
  const offset =
    event.key === 'ArrowLeft' ? -1 : event.key === 'ArrowRight' ? 1 : event.key === 'ArrowUp' ? -columns : columns
  const nextIndex = Math.max(0, Math.min(cards.length - 1, currentIndex + offset))
  if (nextIndex === currentIndex) return
  event.preventDefault()
  cards[nextIndex].focus()
}

function configurationValues(secret: boolean): Record<string, unknown> {
  if (!selectedOption) return {}
  const declared = new Set(
    selectedOption.descriptor.config_fields.filter((field) => field.secret === secret).map((field) => field.key),
  )
  return Object.fromEntries(
    Object.entries(form.values).filter(
      ([key, value]) =>
        declared.has(key) &&
        value !== undefined &&
        (!secret || (value !== null && (typeof value !== 'string' || value.length > 0))),
    ),
  )
}

async function reviewConfiguration(): Promise<void> {
  if (!selectedOption || (!form.baseUrl.trim() && !supportsConfigValidation)) return
  reviewRequestId += 1
  const requestId = reviewRequestId
  reviewing = true
  previewFailure = ''
  preview = undefined
  try {
    const result = await admin.providers.previewConfiguration({
      vendor_id: selectedOption.descriptor.provider_id,
      channel: selectedOption.channel.id,
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

async function saveProvider(): Promise<void> {
  if (!selectedOption || !previewAccepted || !preview) return
  if (!form.name.trim() || (oauthProvider && !oauthReady)) return
  const credentials = configurationValues(true)
  const credential: CreateProvider['credential'] = oauthProvider
    ? { type: 'none' }
    : Object.keys(credentials).length > 0
      ? { type: 'fields', values: credentials }
      : { type: 'none' }
  const input: CreateProvider = {
    name: form.name.trim(),
    source: {
      type: 'custom',
      vendor: selectedOption.descriptor.provider_id,
      channel: selectedOption.channel.id,
      protocol: form.protocol || selectedOption.channel.protocol || undefined,
      base_url: preview.base_url,
    },
    credential,
    vendor_options: configurationValues(false),
    use_proxy: form.useProxy,
  }

  saving = true
  try {
    const savedProvider =
      oauthProvider && oauthSessionId
        ? await admin.providers.createOAuth(oauthSessionId, input)
        : await admin.providers.create(input)
    oauthAuthorization?.consume()
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['models'] }),
    ])
    toast.success(m.provider_editor_service_connected())
    onSaved?.(savedProvider)
    open = false
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}
</script>

{#snippet providerOptionCard(option: ProviderOption)}
  <button
    type="button"
    data-provider-option
    data-active={focusedOptionKey === option.key}
    aria-current={focusedOptionKey === option.key ? 'true' : undefined}
    class="group flex h-full min-h-28 flex-col items-start gap-2.5 rounded-xl border bg-card p-3 text-left transition-[border-color,box-shadow,background-color] hover:border-primary/45 hover:bg-muted/30 hover:shadow-sm focus-visible:outline-none focus-visible:ring-3 focus-visible:ring-ring/50 data-[active=true]:border-primary/45 data-[active=true]:bg-muted/30 data-[active=true]:shadow-sm"
    onfocus={() => (focusedOptionKey = option.key)}
    onkeydown={handleProviderOptionKeydown}
    onclick={() => void chooseOption(option)}>
    <div class="flex w-full items-start gap-3">
      <ProviderMark
        icon={option.descriptor.catalog_id ?? option.descriptor.provider_id}
        name={optionLabel(option)}
        logo={option.descriptor.catalog_id ?? option.descriptor.provider_id} />
      <div class="min-w-0 flex-1">
        <p class="line-clamp-2 text-pretty font-medium leading-snug">{optionLabel(option)}</p>
        <p class="mt-1 line-clamp-2 text-xs leading-relaxed text-muted-foreground">
          {optionDescription(option, localeState.current)}
        </p>
      </div>
    </div>
  </button>
{/snippet}

<Sheet.Root bind:open onOpenChange={handleOpenChange}>
  <Sheet.Content
    side="right"
    class="{step === 'select'
      ? 'provider-overlay-content'
      : 'route-overlay-content-md'} w-full! gap-0 overflow-hidden p-0"
    closeLabel={m.provider_editor_close_service_setup()}>
    <Sheet.Header class="border-b">
      <Sheet.Title
        >{step === 'select' ? m.common_connect_service() : m.provider_editor_connection_details()}</Sheet.Title>
      <Sheet.Description>
        {step === 'select'
          ? m.provider_editor_choose_ai_service_how_want_sign()
          : m.provider_editor_configuration_help()}
      </Sheet.Description>
      <Tabs.Root value={step} onValueChange={changeStep} class="mt-3">
        <Tabs.List aria-label={m.provider_editor_connection_setup_steps()}>
          <Tabs.Trigger value="select">{m.provider_editor_choose_service()}</Tabs.Trigger>
          <Tabs.Trigger value="configure" disabled={!selectedOption}
            >{m.provider_editor_connection_details()}</Tabs.Trigger>
        </Tabs.List>
      </Tabs.Root>
    </Sheet.Header>

    {#if step === 'select'}
      <div class="route-overlay-body">
        <div
          data-provider-toolbar
          class="sticky -top-4 z-10 -mx-4 -mt-4 mb-4 flex items-center gap-2 border-b bg-popover px-4 py-4">
          <InputGroup.Root class="min-w-0 flex-1">
            <InputGroup.Input
              aria-label={m.provider_editor_search_services_sign_methods()}
              bind:value={search}
              placeholder={m.common_search_services()} />
            <InputGroup.Addon><SearchIcon /></InputGroup.Addon>
          </InputGroup.Root>
          <Button
            type="button"
            variant="outline"
            class="shrink-0"
            onclick={refreshServices}
            disabled={refreshingServices}>
            {#if refreshingServices}<Spinner data-icon="inline-start" />{:else}<RefreshCwIcon
                data-icon="inline-start" />{/if}
            {m.provider_editor_update_service_list()}
          </Button>
        </div>
        {#if providerDescriptorsQuery.isError}
          <Alert.Root variant="destructive">
            <Alert.Title>{m.provider_config_plugins_load_failed()}</Alert.Title>
            <Alert.Description>{localizeBackendErrorMessage(providerDescriptorsQuery.error)}</Alert.Description>
          </Alert.Root>
        {:else if providerDescriptorsQuery.isPending}
          <div class="grid min-h-48 place-items-center"><Spinner /></div>
        {:else if providerOptions.length > 0}
          <div
            data-provider-grid
            role="group"
            aria-label={m.provider_editor_available_services()}
            class="provider-picker-grid">
            {#each providerOptions as option (option.key)}{@render providerOptionCard(option)}{/each}
          </div>
        {:else}
          <Empty.Root class="min-h-48 border border-dashed">
            <Empty.Header>
              <Empty.Media variant="icon"><SearchIcon /></Empty.Media>
              <Empty.Title>
                {search.trim() ? m.provider_editor_no_matching_services() : m.provider_config_no_plugins()}
              </Empty.Title>
              <Empty.Description>
                {search.trim()
                  ? m.provider_editor_try_another_service_name_sign_method()
                  : m.provider_config_no_plugins_help()}
              </Empty.Description>
            </Empty.Header>
            <Empty.Content>
              {#if search.trim()}
                <Button type="button" variant="outline" size="sm" onclick={() => (search = '')}>
                  {m.provider_editor_clear_search()}
                </Button>
              {:else}
                <Button href="/vendor-plugins" variant="outline" size="sm">
                  {m.provider_config_manage_plugins()}
                </Button>
              {/if}
            </Empty.Content>
          </Empty.Root>
        {/if}
      </div>
      <Sheet.Footer class="route-overlay-footer flex-row justify-start">
        <Sheet.Close
          type="button"
          class={buttonVariants({ variant: 'outline' })}
          onclick={() => void oauthAuthorization?.cancel()}>
          {m.common_cancel()}
        </Sheet.Close>
      </Sheet.Footer>
    {:else if selectedOption}
      <form
        class="route-overlay-form"
        onsubmit={(event) => {
          event.preventDefault()
          void saveProvider()
        }}>
        <div class="route-overlay-body">
          <Field.Group class="grid gap-6 sm:grid-cols-2">
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
            {#if selectedOption.channel.protocols && selectedOption.channel.protocols.length > 1}
              <Field.Field size="fill" class="sm:col-span-2">
                <Field.Label for="provider-protocol">{m.common_protocol()}</Field.Label>
                <Select.Root
                  type="single"
                  value={form.protocol}
                  onValueChange={(value: string) => {
                    form.protocol = value
                    configurationChanged()
                  }}>
                  <Select.Trigger id="provider-protocol" class="w-full">
                    {selectedOption.channel.protocols.find((option) => option.value === form.protocol)?.label ??
                      form.protocol}
                  </Select.Trigger>
                  <Select.Content>
                    <Select.Group>
                      {#each selectedOption.channel.protocols as option (option.value)}
                        <Select.Item value={option.value}>{option.label}</Select.Item>
                      {/each}
                    </Select.Group>
                  </Select.Content>
                </Select.Root>
              </Field.Field>
            {/if}
            <ProviderConfigFields
              fields={configFields}
              bind:values={form.values}
              satisfiedSecretFields={oauthSessionSecretFields}
              issues={previewIssues}
              idPrefix={`new-provider-${selectedOption.key}`}
              onChanged={configurationChanged} />
          </Field.Group>

          {#if oauthProvider}
            <ProviderOAuthAuthorization
              class="mt-6"
              bind:this={oauthAuthorization}
              vendorId={selectedOption.descriptor.provider_id}
              channel={selectedOption.channel.id}
              flow={selectedOption.channel.auth!.flow}
              configuration={oauthConfiguration}
              useProxy={form.useProxy}
              mode="connect"
              providerName={defaultProviderName(selectedOption)}
              onStateChange={(sessionId: string | undefined, ready: boolean) => {
                oauthSessionId = sessionId
                oauthReady = ready
                invalidatePreview()
              }} />
          {/if}

          <Field.Field orientation="horizontal" class="mt-6 min-h-10 justify-between rounded-lg border px-3 py-2">
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

          <section class="mt-6 rounded-xl border p-4" aria-labelledby="new-provider-network-review-title">
            <div class="flex flex-wrap items-start justify-between gap-3">
              <div>
                <h3 id="new-provider-network-review-title" class="font-medium">{m.provider_config_review_title()}</h3>
                <p class="mt-1 text-sm text-muted-foreground">{m.provider_config_review_help()}</p>
              </div>
              <Button
                type="button"
                variant="outline"
                disabled={reviewing || (!form.baseUrl.trim() && !supportsConfigValidation)}
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
        </div>

        <Sheet.Footer class="route-overlay-footer flex-row justify-between sm:justify-between">
          <Button type="button" variant="outline" onclick={() => void goBack()}>{m.provider_editor_back()}</Button>
          <div class="flex items-center gap-2">
            <Sheet.Close
              type="button"
              class={buttonVariants({ variant: 'outline' })}
              onclick={() => void oauthAuthorization?.cancel()}>
              {m.common_cancel()}
            </Sheet.Close>
            <Button
              type="submit"
              disabled={saving || !form.name.trim() || !previewAccepted || (oauthProvider && !oauthReady)}>
              {#if saving}<Spinner data-icon="inline-start" />{/if}
              {m.provider_editor_connect()}
            </Button>
          </div>
        </Sheet.Footer>
      </form>
    {/if}
  </Sheet.Content>
</Sheet.Root>
