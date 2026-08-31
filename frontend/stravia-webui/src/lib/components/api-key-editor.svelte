<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { useQueryClient } from '@tanstack/svelte-query'
import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down'
import CopyIcon from '@lucide/svelte/icons/copy'
import EyeIcon from '@lucide/svelte/icons/eye'
import EyeOffIcon from '@lucide/svelte/icons/eye-off'
import { toast } from 'svelte-sonner'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import type { ApiKey, Route } from '$lib/types'
import PageHeader from '$lib/components/page-header.svelte'
import * as Field from '$lib/components/ui/field'
import { Button, buttonVariants } from '$lib/components/ui/button'
import * as Command from '$lib/components/ui/command'
import { Input } from '$lib/components/ui/input'
import * as Popover from '$lib/components/ui/popover'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'

interface KeyForm {
  key: string
  customKey: boolean
  name: string
  concurrencyLimit: string
  expiresAt: string
  enabled: boolean
  mcpAccessEnabled: boolean
  transparentInjectionEnabled: boolean
  injectWebSearch: boolean
  injectMediaUnderstanding: boolean
  modelIds: string[]
}

interface Props {
  open?: boolean
  apiKey?: ApiKey
  models: Route[]
  webSearchEnabled: boolean
  mediaUnderstandingEnabled: boolean
  presentation?: 'sheet' | 'page'
  onSaved?: () => void | Promise<void>
}

let {
  open = $bindable(false),
  apiKey,
  models,
  webSearchEnabled,
  mediaUnderstandingEnabled,
  presentation = 'sheet',
  onSaved,
}: Props = $props()
const queryClient = useQueryClient()
let form = $state(keyForm())
let saving = $state(false)
let createdSecret = $state<string>()
let secretVisible = $state(false)
let modelPickerOpen = $state(false)
const allowAllModels = $derived(form.modelIds.length === 0)
const allowedModels = $derived(models.filter((model) => form.modelIds.includes(model.id)))
const unallowedModels = $derived(models.filter((model) => !form.modelIds.includes(model.id)))

function keyForm(source: ApiKey | undefined = apiKey): KeyForm {
  return {
    key: source?.key ?? '',
    customKey: source != null,
    name: source?.name ?? '',
    concurrencyLimit: source?.concurrency_limit ? String(source.concurrency_limit) : '',
    expiresAt: source?.expires_at ? toDateTimeLocal(source.expires_at) : '',
    enabled: source?.is_enabled ?? true,
    mcpAccessEnabled: source?.mcp_access_enabled ?? false,
    transparentInjectionEnabled: source?.transparent_injection_enabled ?? false,
    injectWebSearch: source?.inject_web_search ?? false,
    injectMediaUnderstanding: source?.inject_media_understanding ?? false,
    modelIds: source?.model_ids ?? [],
  }
}

function toDateTimeLocal(value: string): string {
  const date = new Date(value)
  if (Number.isNaN(date.valueOf())) return ''
  const offset = date.getTimezoneOffset() * 60_000
  return new Date(date.valueOf() - offset).toISOString().slice(0, 16)
}

function positiveInteger(value: string): number | undefined {
  const number = Number.parseInt(value, 10)
  return Number.isFinite(number) && number > 0 ? number : undefined
}

function setModelChecked(modelId: string, checked: boolean): void {
  if (checked) {
    if (!form.modelIds.includes(modelId)) form.modelIds = [...form.modelIds, modelId]
    return
  }
  if (form.modelIds.length > 1) form.modelIds = form.modelIds.filter((id) => id !== modelId)
}

function setAllowAllModels(checked: boolean): void {
  if (checked) {
    form.modelIds = []
    modelPickerOpen = false
    return
  }
  if (models.length > 0) form.modelIds = models.map((model) => model.id)
}

async function copySecret(secret: string): Promise<void> {
  if (!secret) return
  try {
    await navigator.clipboard.writeText(secret)
    toast.success(m.api_key_editor_api_key_copied())
  } catch {
    toast.error(m.api_key_editor_not_copy_api_key())
  }
}

async function saveKey(): Promise<void> {
  if (!form.name.trim()) {
    toast.error(m.api_key_editor_api_key_name_required())
    return
  }
  if ((apiKey || form.customKey) && !form.key.trim()) {
    toast.error(m.api_key_editor_api_key_required())
    return
  }

  saving = true
  try {
    const input = {
      key: apiKey || form.customKey ? form.key.trim() : undefined,
      name: form.name.trim(),
      concurrency_limit: positiveInteger(form.concurrencyLimit) ?? null,
      mcp_access_enabled: form.mcpAccessEnabled,
      transparent_injection_enabled: form.transparentInjectionEnabled,
      inject_web_search: form.injectWebSearch,
      inject_media_understanding: form.injectMediaUnderstanding,
      expires_at: form.expiresAt ? new Date(form.expiresAt).toISOString() : undefined,
      model_ids: form.modelIds,
    }
    if (apiKey) {
      await admin.apiKeys.update(apiKey.id, { ...input, is_enabled: form.enabled })
      if (presentation === 'sheet') open = false
      toast.success(m.api_key_editor_api_key_saved())
    } else {
      const created = await admin.apiKeys.create(input)
      createdSecret = created.key
      toast.success(m.api_key_editor_api_key_created_copy_now_not_shown_again())
    }
    await queryClient.invalidateQueries({ queryKey: ['api-keys'] })
    if (apiKey) await onSaved?.()
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
}
</script>

{#snippet keyFields()}
  <Field.Group>
    <div class="flex flex-wrap items-end gap-x-4 gap-y-3">
      <Field.Field orientation="vertical" class="w-auto min-w-64 flex-1">
        <Field.Label for="api-key-name">{m.common_name()}</Field.Label>
        <Input id="api-key-name" bind:value={form.name} required />
      </Field.Field>
      {#if apiKey}
        <Field.Field orientation="horizontal" class="w-28 flex-none">
          <Switch id="api-key-enabled" bind:checked={form.enabled} />
          <Field.Label for="api-key-enabled">{m.common_enabled_status()}</Field.Label>
        </Field.Field>
      {/if}
    </div>
    {#if !apiKey}
      <Field.Field orientation="horizontal">
        <Switch id="api-key-custom" bind:checked={form.customKey} />
        <Field.Label for="api-key-custom" hint={m.api_key_editor_custom_api_key_help()}>
          {m.api_key_editor_custom_api_key()}
        </Field.Label>
      </Field.Field>
    {/if}
    {#if apiKey || form.customKey}
      <Field.Field orientation="vertical">
        <Field.Label for="api-key-secret">{m.api_key_editor_api_key()}</Field.Label>
        <div class="flex gap-2">
          <Input
            id="api-key-secret"
            class="font-technical"
            bind:value={form.key}
            type={secretVisible ? 'text' : 'password'}
            autocomplete="off"
            required />
          <Button
            type="button"
            size="icon"
            variant="outline"
            aria-label={secretVisible ? m.api_key_editor_hide_api_key() : m.api_key_editor_show_api_key()}
            onclick={() => (secretVisible = !secretVisible)}>
            {#if secretVisible}<EyeOffIcon />{:else}<EyeIcon />{/if}
          </Button>
          <Button type="button" variant="outline" onclick={() => void copySecret(form.key)}>
            <CopyIcon data-icon="inline-start" />{m.common_copy()}
          </Button>
        </div>
        <Field.Description>{m.api_key_editor_api_key_help()}</Field.Description>
      </Field.Field>
    {/if}
    <Field.Group class="flex-row flex-wrap items-center gap-4">
      <Field.Field orientation="horizontal" class="w-auto min-w-52 flex-1">
        <Switch
          id="api-key-allow-all-models"
          checked={allowAllModels}
          disabled={models.length === 0}
          onCheckedChange={setAllowAllModels} />
        <Field.Label for="api-key-allow-all-models" hint={m.api_key_editor_unrestricted_model_scope_help()}>
          {m.api_key_editor_allow_all_models()}
        </Field.Label>
      </Field.Field>
      {#if !allowAllModels}
        <Field.Field class="w-auto min-w-52 flex-1">
          <Popover.Root bind:open={modelPickerOpen}>
            <Popover.Trigger>
              {#snippet child({ props })}
                <Button
                  {...props}
                  id="api-key-model-picker"
                  type="button"
                  variant="outline"
                  class="w-full min-w-0 justify-between font-normal"
                  role="combobox"
                  aria-label={m.api_key_editor_select_allowed_models()}
                  aria-expanded={modelPickerOpen}>
                  <span class="truncate text-left">
                    {m.api_key_editor_allowed_model_count({ count: allowedModels.length })}
                  </span>
                  <ChevronsUpDownIcon data-icon="inline-end" class="ml-auto opacity-50" />
                </Button>
              {/snippet}
            </Popover.Trigger>
            <Popover.Content align="start" class="w-(--bits-popover-anchor-width) p-0">
              <Command.Root label={m.api_key_editor_select_allowed_models()}>
                <Command.Input
                  placeholder={m.api_key_editor_search_models()}
                  aria-label={m.api_key_editor_search_models()} />
                <Command.List>
                  <Command.Empty>{m.api_key_editor_no_models_found()}</Command.Empty>
                  <Command.Group heading={m.api_key_editor_allowed_models()}>
                    {#each allowedModels as model (model.id)}
                      <Command.Item
                        value={model.id}
                        keywords={[model.name]}
                        data-checked
                        disabled={allowedModels.length === 1}
                        onSelect={() => setModelChecked(model.id, false)}>
                        <span class="truncate">{model.name}</span>
                      </Command.Item>
                    {/each}
                  </Command.Group>
                  {#if allowedModels.length > 0 && unallowedModels.length > 0}
                    <Command.Separator />
                  {/if}
                  <Command.Group heading={m.api_key_editor_unallowed_models()}>
                    {#each unallowedModels as model (model.id)}
                      <Command.Item
                        value={model.id}
                        keywords={[model.name]}
                        onSelect={() => setModelChecked(model.id, true)}>
                        <span class="truncate">{model.name}</span>
                      </Command.Item>
                    {/each}
                  </Command.Group>
                </Command.List>
              </Command.Root>
            </Popover.Content>
          </Popover.Root>
        </Field.Field>
      {/if}
    </Field.Group>
    <div class="flex flex-wrap items-end gap-4">
      <Field.Field orientation="vertical" class="w-auto min-w-52 flex-1">
        <Field.Label for="api-key-concurrency-limit" hint={m.api_key_editor_concurrency_limit_help()}>
          {m.api_key_editor_maximum_concurrent_executions()}
        </Field.Label>
        <Input
          id="api-key-concurrency-limit"
          bind:value={form.concurrencyLimit}
          min="1"
          step="1"
          type="number"
          placeholder={m.api_key_editor_unlimited()} />
      </Field.Field>
      <Field.Field orientation="vertical" class="w-auto min-w-52 flex-1">
        <Field.Label for="api-key-expires" hint={m.api_key_editor_never_expires_placeholder()}>
          {m.api_key_editor_expires()}
        </Field.Label>
        <Input
          id="api-key-expires"
          bind:value={form.expiresAt}
          type="datetime-local"
          placeholder={m.api_key_editor_never_expires_placeholder()} />
      </Field.Field>
    </div>
    <Field.Group class="gap-0!">
      <Field.Field orientation="horizontal">
        <Switch id="api-key-mcp-access" class="-mt-2" bind:checked={form.mcpAccessEnabled} />
        <Field.Content>
          <Field.Label for="api-key-mcp-access" hint={m.api_key_editor_lets_key_call_stravia_tools_mcp()}>
            {m.api_key_editor_allow_mcp_clients()}
          </Field.Label>
        </Field.Content>
      </Field.Field>
      <Field.Field orientation="horizontal">
        <Switch
          id="api-key-transparent-injection"
          class="-mt-2"
          bind:checked={form.transparentInjectionEnabled} />
        <Field.Content>
          <Field.Label for="api-key-transparent-injection" hint={m.api_key_editor_transparent_injection_help()}>
            {m.api_key_editor_transparent_injection()}
          </Field.Label>
        </Field.Content>
      </Field.Field>
    </Field.Group>
    {#if form.transparentInjectionEnabled}
      <Field.Set>
        <Field.Field>
          <Field.Label hint={m.api_key_editor_choose_automatic_capabilities()}>
            {m.api_key_editor_automatically_exposed_capabilities()}
          </Field.Label>
        </Field.Field>
        <Field.Group class="flex-row flex-wrap gap-x-4 gap-y-3">
          <Field.Field orientation="horizontal" class="min-w-52 flex-1 basis-52">
            <Switch
              id="api-key-inject-media-understanding"
              class="-mt-2.5"
              bind:checked={form.injectMediaUnderstanding}
              disabled={!mediaUnderstandingEnabled} />
            <Field.Content>
              <Field.Label for="api-key-inject-media-understanding">
                {m.api_key_editor_media_understanding()}
              </Field.Label>
              {#if !mediaUnderstandingEnabled}
                <a
                  href={resolve('/media-understanding')}
                  class="text-sm text-muted-foreground underline underline-offset-2 hover:text-foreground">
                  {m.api_key_editor_platform_capability_disabled()}
                </a>
              {/if}
            </Field.Content>
          </Field.Field>
          <Field.Field orientation="horizontal" class="min-w-52 flex-1 basis-52">
            <Switch
              id="api-key-inject-web-search"
              class="-mt-2.5"
              bind:checked={form.injectWebSearch}
              disabled={!webSearchEnabled} />
            <Field.Content>
              <Field.Label for="api-key-inject-web-search">
                {m.api_key_editor_web_search()}
              </Field.Label>
              {#if !webSearchEnabled}
                <a
                  href={resolve('/web-search')}
                  class="text-sm text-muted-foreground underline underline-offset-2 hover:text-foreground">
                  {m.api_key_editor_platform_capability_disabled()}
                </a>
              {/if}
            </Field.Content>
          </Field.Field>
        </Field.Group>
      </Field.Set>
    {/if}
  </Field.Group>
{/snippet}

{#snippet editorForm(pageMode: boolean)}
  <form
    class={pageMode ? 'flex flex-1 flex-col gap-6' : 'route-overlay-form'}
    onsubmit={(event) => {
      event.preventDefault()
      void saveKey()
    }}>
    {#if pageMode}
      <section class="route-section" aria-labelledby="api-key-credentials-title">
        <div class="route-section-header">
          <div>
            <h2 id="api-key-credentials-title" class="route-section-title">{m.api_keys_client_credentials()}</h2>
            <p class="route-section-description">{m.api_key_editor_configuration_summary()}</p>
          </div>
        </div>
        {@render keyFields()}
      </section>
      <div
        class="sticky bottom-0 z-20 mt-auto flex translate-y-2 justify-end gap-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
        <Button href={resolve('/api-keys')} variant="outline">{m.common_cancel()}</Button>
        <Button type="submit" disabled={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.api_key_editor_save_api_key()}
        </Button>
      </div>
    {:else}
      <div class="route-overlay-body">{@render keyFields()}</div>
      <Sheet.Footer class="route-overlay-footer"
        ><Sheet.Close type="button" class={buttonVariants({ variant: 'outline' })}>{m.common_cancel()}</Sheet.Close
        ><Button type="submit" disabled={saving}
          >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.api_key_editor_save_api_key()}</Button
        ></Sheet.Footer>
    {/if}
  </form>
{/snippet}

{#if presentation === 'page'}
  <div class="route-page mx-auto min-h-[calc(100svh-5rem)] w-full max-w-[90rem]">
    <PageHeader
      eyebrow={m.common_setup()}
      title={m.api_key_editor_edit_api_key()}
      description={m.api_key_editor_configuration_summary()} />
    {@render editorForm(true)}
  </div>
{:else}
  <Sheet.Root bind:open>
    <Sheet.Content
      side="right"
      class="route-overlay-content w-full! gap-0 overflow-hidden p-0"
      closeLabel={m.api_key_editor_close_api_key_editor()}>
      <Sheet.Header class="border-b">
        <Sheet.Title>{apiKey ? m.api_key_editor_edit_api_key() : m.common_create_api_key()}</Sheet.Title>
        <Sheet.Description>{m.api_key_editor_configuration_summary()}</Sheet.Description>
      </Sheet.Header>

      {#if createdSecret}
        <div class="route-overlay-body">
          <Field.Group>
            <Field.Field size="fill">
              <Field.Label for="created-api-key">{m.api_key_editor_copy_api_key_now()}</Field.Label>
              <div class="flex flex-col gap-2 sm:flex-row">
                <Input id="created-api-key" class="font-technical text-lg" value={createdSecret} readonly /><Button
                  type="button"
                  variant="outline"
                  onclick={() => void copySecret(createdSecret ?? '')}
                  ><CopyIcon data-icon="inline-start" />{m.common_copy()}</Button>
              </div>
              <Field.Description>{m.api_key_editor_api_key_manage_later()}</Field.Description>
            </Field.Field>
          </Field.Group>
        </div>
        <Sheet.Footer class="route-overlay-footer"
          ><Sheet.Close class={buttonVariants({ variant: 'default' })}>{m.api_key_editor_done()}</Sheet.Close
          ></Sheet.Footer>
      {:else}
        {@render editorForm(false)}
      {/if}
    </Sheet.Content>
  </Sheet.Root>
{/if}
