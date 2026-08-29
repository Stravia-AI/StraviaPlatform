<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { createQuery, useQueryClient } from '@tanstack/svelte-query'
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'
import RefreshCwIcon from '@lucide/svelte/icons/refresh-cw'
import SearchIcon from '@lucide/svelte/icons/search'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { localeState } from '$lib/localization.svelte'
import { providerCredentialFieldLabel } from '$lib/provider-credential-labels'
import {
  buildProviderOptions,
  defaultProviderName,
  oauthDriverKey,
  optionDescription,
  optionLabel,
  providerNameAfterOptionChange,
  type ProviderOption,
} from '$lib/provider-options'
import { PROTOCOL_TABLE, resolveProtocol } from '$lib/protocol'
import type { CatalogProvider, CreateProvider, Provider, ProviderProtocol, VendorCredentialField } from '$lib/types'
import ProviderOAuthAuthorization from '$lib/components/provider-oauth-authorization.svelte'
import { Badge } from '$lib/components/ui/badge'
import { Button, buttonVariants } from '$lib/components/ui/button'
import ProviderMark from '$lib/components/provider-mark.svelte'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import * as Tabs from '$lib/components/ui/tabs'
import { Textarea } from '$lib/components/ui/textarea'

interface ProviderForm {
  name: string
  vendor: string
  protocol: ProviderProtocol
  baseUrl: string
  apiKey: string
  useProxy: boolean
  authMode: 'apikey' | 'oauth'
  presetKey: string
  channel: string
  modelsSource: string
  staticModels: string
}

interface Props {
  open?: boolean
  presets: CatalogProvider[]
  onSaved?: (provider: Provider) => void
}

let { open = $bindable(false), presets, onSaved }: Props = $props()
let step = $state<'select' | 'configure'>('select')
let search = $state('')
let focusedOptionKey = $state('')
let selectedOptionKey = $state('')
let form = $state<ProviderForm>({
  name: '',
  vendor: '',
  protocol: 'openai-compatible',
  baseUrl: '',
  apiKey: '',
  useProxy: false,
  authMode: 'apikey',
  presetKey: 'custom',
  channel: 'default',
  modelsSource: '',
  staticModels: '',
})
let oauthSessionId = $state<string>()
let oauthReady = $state(false)
let oauthAuthorization = $state<{
  cancel: () => Promise<void>
  consume: () => void
  updateProxy: (useProxy: boolean) => Promise<void>
}>()
let saving = $state(false)
let refreshingCatalog = $state(false)
let adapterCredentials = $state<Record<string, string>>({})
let visibleCredentialKeys = $state<Record<string, boolean>>({})

function toggleCredentialVisibility(key: string): void {
  visibleCredentialKeys[key] = !visibleCredentialKeys[key]
}

const queryClient = useQueryClient()
const vendorMetadataQuery = createQuery(() => ({
  queryKey: ['vendor-metadata'],
  queryFn: admin.providers.vendors,
}))
const options = $derived(buildProviderOptions(presets))
const selectedOption = $derived(options.find((option) => option.key === selectedOptionKey))
const credentialFields = $derived<VendorCredentialField[]>(
  selectedOption
    ? (vendorMetadataQuery.data?.find((vendor) => vendor.id === selectedOption.preset.vendor_id)?.credentialFields ?? [])
    : [],
)
const usesDynamicCredentials = $derived(
  credentialFields.length > 1 || credentialFields.some((field) => field.key !== 'apiKey'),
)
const previewsBaseUrl = $derived(
  Boolean(
    selectedOption &&
      !selectedOption.isCustom &&
      !selectedOption.channel.base_url.trim() &&
      usesDynamicCredentials,
  ),
)
const baseUrlCredentials = $derived(
  Object.fromEntries(
    credentialFields
      .filter((field) => !field.secret)
      .map((field) => [field.key, adapterCredentials[field.key]?.trim() ?? '']),
  ),
)
const baseUrlPreviewQuery = createQuery(() => ({
  queryKey: ['provider-base-url-preview', selectedOption?.preset.vendor_id, baseUrlCredentials],
  queryFn: () =>
    admin.providers.previewBaseUrl(selectedOption!.preset.vendor_id, baseUrlCredentials),
  enabled:
    previewsBaseUrl &&
    Object.values(baseUrlCredentials).some((value) => value.length > 0),
  retry: false,
}))
const assembledBaseUrl = $derived(baseUrlPreviewQuery.data?.base_url ?? '')
const hasMissingCredential = $derived(
  usesDynamicCredentials && credentialFields.some((field) => field.required && !adapterCredentials[field.key]?.trim()),
)
const providerOptions = $derived.by(() => {
  const query = search.trim().toLocaleLowerCase(localeState.current)
  return options
    .filter((option) => {
      if (!query) return true
      const auth = option.authMode === 'oauth' ? 'oauth account 账号' : 'api key'
      const text = `${option.preset.id} ${option.preset.name} ${option.channel.id} ${option.channel.label} ${auth}`
      return text.toLocaleLowerCase(localeState.current).includes(query)
    })
    .sort((left, right) => Number(right.isCustom) - Number(left.isCustom))
})
const availableProtocols = $derived(
  selectedOption?.isCustom || !selectedOption
    ? PROTOCOL_TABLE
    : selectedOption.protocols.map(
        ({ protocol }) => PROTOCOL_TABLE.find((entry) => entry.id === protocol) ?? PROTOCOL_TABLE[0],
      ),
)
const apiKeyRequired = $derived(
  Boolean(selectedOption && !selectedOption.isCustom && selectedOption.credentialMode === 'setup_token'),
)

function handleOpenChange(nextOpen: boolean): void {
  open = nextOpen
  if (!nextOpen) void oauthAuthorization?.cancel()
}

async function chooseOption(option: ProviderOption): Promise<void> {
  await oauthAuthorization?.cancel()
  const firstProtocol = option.protocols[0] ?? { protocol: resolveProtocol(option.preset.protocol), baseUrl: '' }
  const name = providerNameAfterOptionChange(form.name, selectedOption, option, localeState.current)
  selectedOptionKey = option.key
  form = {
    name,
    vendor: option.isCustom ? '' : option.presetKey,
    protocol: firstProtocol.protocol,
    baseUrl: firstProtocol.baseUrl,
    apiKey: '',
    useProxy: form.useProxy,
    authMode: option.authMode,
    presetKey: option.presetKey,
    channel: option.channelKey,
    modelsSource: option.isCustom ? '' : 'catalog',
    staticModels: '',
  }
  adapterCredentials = {}
  visibleCredentialKeys = {}
  step = 'configure'
}

async function goBack(): Promise<void> {
  await oauthAuthorization?.cancel()
  step = 'select'
}

function changeStep(value: string): void {
  if (value === 'select') {
    void goBack()
  } else if (selectedOption) {
    step = 'configure'
  }
}

async function refreshCatalog(): Promise<void> {
  refreshingCatalog = true
  try {
    const summary = await admin.catalog.refresh()
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['catalog-providers'] }),
      queryClient.invalidateQueries({ queryKey: ['provider-models'] }),
    ])
    toast.success(
      m.provider_editor_catalog_refresh_summary({
        provider_count: summary.provider_count,
        model_count: summary.model_count,
      }),
    )
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    refreshingCatalog = false
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

function updateProtocol(protocol: ProviderProtocol): void {
  form.protocol = protocol
  const endpoint = selectedOption?.protocols.find((item) => item.protocol === protocol)
  if (endpoint) form.baseUrl = endpoint.baseUrl
}

async function saveProvider(): Promise<void> {
  if (!selectedOption || !form.name.trim() || (!form.baseUrl.trim() && !assembledBaseUrl)) return
  if (apiKeyRequired && !form.apiKey.trim()) return
  if (hasMissingCredential) return
  if (form.authMode === 'oauth' && !oauthReady) return

  const credential: CreateProvider['credential'] =
    selectedOption.credentialMode === 'setup_token'
      ? { type: 'setup_token', value: form.apiKey.trim() }
      : form.authMode === 'apikey' && usesDynamicCredentials
        ? {
            type: 'fields',
            values: Object.fromEntries(
              credentialFields.map((field) => [field.key, adapterCredentials[field.key]?.trim() ?? '']),
            ),
          }
      : form.authMode === 'apikey' && form.apiKey.trim()
        ? { type: 'api_key', value: form.apiKey.trim() }
        : { type: 'none' }
  const input: CreateProvider = {
    name: form.name.trim(),
    source: selectedOption.isCustom
      ? {
          type: 'custom',
          vendor: form.vendor.trim() || undefined,
          protocol: form.protocol,
          base_url: form.baseUrl.trim(),
          models_source: form.modelsSource.trim() || undefined,
          static_models: form.staticModels.trim() || undefined,
        }
      : {
          type: 'catalog',
          provider_id: selectedOption.presetKey,
          channel_id: selectedOption.channelKey,
          fingerprint: selectedOption.channel.fingerprint,
          base_url_override: form.baseUrl.trim() === selectedOption.channel.base_url ? undefined : form.baseUrl.trim(),
        },
    credential,
    use_proxy: form.useProxy,
  }

  saving = true
  try {
    const savedProvider =
      form.authMode === 'oauth' && oauthSessionId
        ? await admin.providers.createOAuth(oauthSessionId, input)
        : await admin.providers.create(input)
    oauthAuthorization?.consume()
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ['providers'] }),
      queryClient.invalidateQueries({ queryKey: ['models'] }),
    ])
    toast.success(m.provider_editor_model_service_connected())
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
      <ProviderMark icon={option.preset.id} name={optionLabel(option, localeState.current)} catalog />
      <div class="min-w-0 flex-1">
        <p class="line-clamp-2 text-pretty font-medium leading-snug">{optionLabel(option, localeState.current)}</p>
        <p class="mt-1 line-clamp-2 text-xs leading-relaxed text-muted-foreground">
          {optionDescription(option, localeState.current)}
        </p>
      </div>
    </div>
    {#if option.isCustom}
      <Badge variant="outline" class="mt-auto">{m.provider_editor_advanced()}</Badge>
    {/if}
  </button>
{/snippet}

<Sheet.Root bind:open onOpenChange={handleOpenChange}>
  <Sheet.Content
    side="right"
    class="{step === 'select'
      ? 'provider-overlay-content'
      : 'route-overlay-content-md'} w-full! gap-0 overflow-hidden p-0"
    closeLabel={m.provider_editor_close_model_service_setup()}>
    <Sheet.Header class="border-b">
      <div class="flex items-center gap-2">
        <div>
          <Sheet.Title>
            {step === 'select' ? m.common_connect_model_service() : m.provider_editor_connection_details()}
          </Sheet.Title>
          <Sheet.Description>
            {step === 'select'
              ? m.provider_editor_choose_ai_service_how_want_sign()
              : m.provider_editor_configuration_help()}
          </Sheet.Description>
          <Tabs.Root value={step} onValueChange={changeStep} class="mt-3">
            <Tabs.List class="grid w-full max-w-72 grid-cols-2" aria-label={m.provider_editor_connection_setup_steps()}>
              <Tabs.Trigger value="select">
                {m.provider_editor_choose_service()}
              </Tabs.Trigger>
              <Tabs.Trigger value="configure" disabled={!selectedOption}>
                {m.provider_editor_connection_details()}
              </Tabs.Trigger>
            </Tabs.List>
          </Tabs.Root>
        </div>
      </div>
    </Sheet.Header>

    {#if step === 'select'}
      <div class="route-overlay-body">
        <div
          data-provider-toolbar
          class="sticky -top-4 z-10 -mx-4 -mt-4 mb-4 flex items-center gap-2 border-b bg-popover px-4 py-4">
          <div class="relative min-w-0 flex-1">
            <SearchIcon
              class="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              aria-label={m.provider_editor_search_services_sign_methods()}
              class="pl-9"
              bind:value={search}
              placeholder={m.common_search_services()} />
          </div>
          <Button
            type="button"
            variant="outline"
            class="shrink-0"
            onclick={() => void refreshCatalog()}
            disabled={refreshingCatalog}>
            <RefreshCwIcon data-icon="inline-start" class={refreshingCatalog ? 'animate-spin' : ''} />
            {m.provider_editor_update_service_list()}
          </Button>
        </div>
        {#if providerOptions.length > 0}
          <div
            data-provider-grid
            role="group"
            aria-label={m.provider_editor_available_services()}
            class="provider-picker-grid">
            {#each providerOptions as option (option.key)}
              {@render providerOptionCard(option)}
            {/each}
          </div>
        {:else}
          <div class="grid min-h-48 place-items-center rounded-xl border border-dashed px-6 text-center">
            <div>
              <SearchIcon class="mx-auto size-5 text-muted-foreground" />
              <p class="mt-3 font-medium">
                {m.provider_editor_no_matching_services()}
              </p>
              <p class="mt-1 text-sm text-muted-foreground">
                {m.provider_editor_try_another_service_name_sign_method()}
              </p>
              <Button type="button" variant="outline" size="sm" class="mt-4" onclick={() => (search = '')}>
                {m.provider_editor_clear_search()}
              </Button>
            </div>
          </div>
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
          <div class="grid gap-6 sm:grid-cols-2">
            <Field.Field size="name" class="sm:col-span-2">
              <Field.Label for="provider-name">{m.common_connection_name()}</Field.Label>
              <Input id="provider-name" bind:value={form.name} required />
            </Field.Field>
            {#if availableProtocols.length > 1}
              <Field.Field size="select">
                <Field.Label for="provider-protocol">{m.common_protocol()}</Field.Label>
                <Select.Root
                  type="single"
                  value={form.protocol}
                  onValueChange={(value) => {
                    const protocol = resolveProtocol(value)
                    if (protocol) updateProtocol(protocol)
                  }}>
                  <Select.Trigger id="provider-protocol" class="w-full"
                    >{PROTOCOL_TABLE.find((item) => item.id === form.protocol)?.displayName}</Select.Trigger>
                  <Select.Content>
                    {#each availableProtocols as protocol (protocol.id)}
                      <Select.Item value={protocol.id}>{protocol.displayName}</Select.Item>
                    {/each}
                  </Select.Content>
                </Select.Root>
              </Field.Field>
            {/if}

            <Field.Field class="justify-end">
              <div class="flex items-center justify-between gap-3 rounded-lg border px-3 py-2.5">
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
            </Field.Field>

            {#if selectedOption.isCustom}
              <Field.Field size="name" class="sm:col-span-2">
                <Field.Label for="provider-vendor">{m.common_service_identifier()}</Field.Label>
                <Input id="provider-vendor" class="font-technical" bind:value={form.vendor} placeholder="custom" />
              </Field.Field>
              <Field.Field size="fill" class="sm:col-span-2">
                <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
                <Input id="provider-base-url" class="font-technical" bind:value={form.baseUrl} type="url" required />
              </Field.Field>
              <Field.Field size="fill" class="sm:col-span-2">
                <Field.Label for="provider-api-key">{m.common_api_key()}</Field.Label>
                <div class="flex gap-2">
                  <Input
                    id="provider-api-key"
                    class="font-technical"
                    bind:value={form.apiKey}
                    type={visibleCredentialKeys['provider-api-key'] ? 'text' : 'password'}
                    autocomplete="off" />
                  <Button
                    type="button"
                    variant="outline"
                    size="icon"
                    onclick={() => toggleCredentialVisibility('provider-api-key')}
                    aria-label={visibleCredentialKeys['provider-api-key'] ? m.common_hide_secret() : m.common_show_secret()}>
                    {#if visibleCredentialKeys['provider-api-key']}<EyeOffIcon />{:else}<EyeIcon />{/if}
                  </Button>
                </div>
              </Field.Field>
              <Field.Field size="fill">
                <Field.Label for="provider-models-source">{m.common_model_list_url()}</Field.Label>
                <Input
                  id="provider-models-source"
                  class="font-technical"
                  bind:value={form.modelsSource}
                  type="url"
                  placeholder="https://api.example.com/v1/models" />
              </Field.Field>
              <Field.Field size="fill">
                <Field.Label for="provider-static-models">{m.common_additional_model_ids()}</Field.Label>
                <Textarea
                  id="provider-static-models"
                  class="min-h-28 font-technical"
                  bind:value={form.staticModels}
                  placeholder={m.provider_editor_one_model_id_per_line()} />
              </Field.Field>
            {:else if form.authMode === 'apikey'}
              {#if selectedOption.channel.base_url.trim().length === 0 && !previewsBaseUrl}
                <Field.Field size="fill" class="sm:col-span-2">
                  <Field.Label for="provider-base-url">{m.common_base_url()}</Field.Label>
                  <Input id="provider-base-url" class="font-technical" bind:value={form.baseUrl} type="url" required />
                </Field.Field>
              {:else if previewsBaseUrl}
                <Field.Field size="fill" class="sm:col-span-2" data-invalid={baseUrlPreviewQuery.isError}>
                  <Field.Label>{m.common_base_url()}</Field.Label>
                  {#if assembledBaseUrl}
                    <Field.Description class="font-technical">{assembledBaseUrl}</Field.Description>
                  {:else if baseUrlPreviewQuery.isError}
                    <Field.Error>{localizeBackendErrorMessage(baseUrlPreviewQuery.error)}</Field.Error>
                  {/if}
                </Field.Field>
              {/if}
              {#if usesDynamicCredentials}
                {#each credentialFields as field (field.key)}
                  {@const credentialId = `provider-credential-${field.key}`}
                  <Field.Field size="fill" class="sm:col-span-2">
                    <Field.Label for={credentialId}>
                      {providerCredentialFieldLabel(
                        selectedOption.preset.vendor_id,
                        field,
                        localeState.current,
                      )}
                    </Field.Label>
                    {#if field.input === 'textarea'}
                      <Textarea
                        id={credentialId}
                        class="min-h-28 font-technical"
                        bind:value={adapterCredentials[field.key]}
                        required={field.required}
                        autocomplete="off" />
                    {:else}
                      <div class="flex gap-2">
                        <Input
                          id={credentialId}
                          class="font-technical"
                          bind:value={adapterCredentials[field.key]}
                          type={field.input === 'password' && !visibleCredentialKeys[credentialId] ? 'password' : 'text'}
                          autocomplete="off"
                          required={field.required} />
                        {#if field.input === 'password'}
                          <Button
                            type="button"
                            variant="outline"
                            size="icon"
                            onclick={() => toggleCredentialVisibility(credentialId)}
                            aria-label={visibleCredentialKeys[credentialId] ? m.common_hide_secret() : m.common_show_secret()}>
                            {#if visibleCredentialKeys[credentialId]}<EyeOffIcon />{:else}<EyeIcon />{/if}
                          </Button>
                        {/if}
                      </div>
                    {/if}
                  </Field.Field>
                {/each}
              {:else}
                <Field.Field size="fill" class="sm:col-span-2">
                  <Field.Label for="provider-api-key" hint={selectedOption.credentialMode === 'setup_token'
                    ? m.provider_editor_sign_method_requires_setup_token()
                    : undefined}>
                    {selectedOption.credentialMode === 'setup_token'
                      ? m.provider_options_setup_token()
                      : m.common_api_key()}
                  </Field.Label>
                  <div class="flex gap-2">
                    <Input
                      id="provider-api-key"
                      class="font-technical"
                      bind:value={form.apiKey}
                      type={visibleCredentialKeys['provider-api-key'] ? 'text' : 'password'}
                      autocomplete="off"
                      required={apiKeyRequired} />
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      onclick={() => toggleCredentialVisibility('provider-api-key')}
                      aria-label={visibleCredentialKeys['provider-api-key'] ? m.common_hide_secret() : m.common_show_secret()}>
                      {#if visibleCredentialKeys['provider-api-key']}<EyeOffIcon />{:else}<EyeIcon />{/if}
                    </Button>
                  </div>
                </Field.Field>
              {/if}
            {:else}
              <ProviderOAuthAuthorization
                class="sm:col-span-2"
                bind:this={oauthAuthorization}
                driver={oauthDriverKey(selectedOption)}
                useProxy={form.useProxy}
                mode="connect"
                providerName={defaultProviderName(selectedOption, localeState.current)}
                onStateChange={(sessionId, ready) => {
                  oauthSessionId = sessionId
                  oauthReady = ready
                }} />
            {/if}
          </div>
        </div>

        <Sheet.Footer class="route-overlay-footer flex-row justify-between sm:justify-between">
          <Button type="button" variant="outline" onclick={() => void goBack()}>
            {m.provider_editor_back()}
          </Button>
          <div class="flex items-center gap-2">
            <Sheet.Close
              type="button"
              class={buttonVariants({ variant: 'outline' })}
              onclick={() => void oauthAuthorization?.cancel()}>
              {m.common_cancel()}
            </Sheet.Close>
            <Button
              type="submit"
              disabled={saving ||
                !form.name.trim() ||
                (!form.baseUrl.trim() && !assembledBaseUrl) ||
                (apiKeyRequired && !form.apiKey.trim()) ||
                hasMissingCredential ||
                (form.authMode === 'oauth' && !oauthReady)}>
              {#if saving}<Spinner data-icon="inline-start" />{/if}
              {m.provider_editor_connect()}
            </Button>
          </div>
        </Sheet.Footer>
      </form>
    {/if}
  </Sheet.Content>
</Sheet.Root>
