<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { resolve } from '$app/paths'
import { useQueryClient } from '@tanstack/svelte-query'
import ChevronsUpDownIcon from '@lucide/svelte/icons/chevrons-up-down'
import CopyIcon from '@lucide/svelte/icons/copy'
import { toast } from 'svelte-sonner'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'

import { effectiveModelDisplayName, logicalModelSecondaryId, sortLogicalModels } from '#lib/logical-model.js'

import type { ApiKey, Route } from '#lib/types.js'
import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import PageHeader from '#lib/components/page-header.svelte'
import SettingRow from '#lib/components/setting-row.svelte'
import * as Field from '#lib/components/ui/field/index.js'
import { Button, buttonVariants } from '#lib/components/ui/button/index.js'
import * as Command from '#lib/components/ui/command/index.js'
import SecretInput from '#lib/components/secret-input.svelte'
import * as InputGroup from '#lib/components/ui/input-group/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { Switch } from '#lib/components/ui/switch/index.js'

interface KeyForm {
  key: string
  customKey: boolean
  name: string
  rpmLimit: string
  expiresAt: string
  enabled: boolean
  mcpAccessEnabled: boolean
  transparentInjectionEnabled: boolean
  injectWebSearch: boolean
  injectMediaUnderstanding: boolean
  injectMediaGeneration: boolean
  modelIds: string[]
}

interface Props {
  open?: boolean
  apiKey?: ApiKey
  models: Route[]
  webSearchEnabled: boolean
  mediaUnderstandingEnabled: boolean
  mediaGenerationStatus: 'enabled' | 'disabled' | 'loading' | 'unavailable'
  presentation?: 'sheet' | 'page'
  onSaved?: () => void | Promise<void>
}

let {
  open = $bindable(false),
  apiKey,
  models,
  webSearchEnabled,
  mediaUnderstandingEnabled,
  mediaGenerationStatus,
  presentation = 'sheet',
  onSaved,
}: Props = $props()

const queryClient = useQueryClient()
let form = $state(keyForm())
let saving = $state(false)
let createdSecret = $state<string>()
const secretResetKey = $derived(`${open}:${apiKey?.id ?? 'new'}`)
let modelPickerOpen = $state(false)
// 草稿离开保护：创建成功后密钥只读展示，不再是草稿。
const baseline = new DraftBaseline(() => form)
const guard = new DraftGuard({ dirty: () => !createdSecret && baseline.dirty, busy: () => saving })
const allowAllModels = $derived(form.modelIds.length === 0)
const sortedModels = $derived(sortLogicalModels(models))
const allowedModels = $derived(sortedModels.filter((model) => form.modelIds.includes(model.id)))
const unallowedModels = $derived(sortedModels.filter((model) => !form.modelIds.includes(model.id)))

function keyForm(source: ApiKey | undefined = apiKey): KeyForm {
  return {
    key: source?.key ?? '',
    customKey: source != null,
    name: source?.name ?? '',
    rpmLimit: source?.rpm_limit ? String(source.rpm_limit) : '',
    expiresAt: source?.expires_at ? toDateTimeLocal(source.expires_at) : '',
    enabled: source?.is_enabled ?? true,
    mcpAccessEnabled: source?.mcp_access_enabled ?? false,
    transparentInjectionEnabled: source?.transparent_injection_enabled ?? false,
    injectWebSearch: source?.inject_web_search ?? false,
    injectMediaUnderstanding: source?.inject_media_understanding ?? false,
    injectMediaGeneration: source?.inject_media_generation ?? false,
    modelIds: source?.model_ids ?? [],
  }
}

function toDateTimeLocal(value: string): string {
  const date = new Date(value)
  if (Number.isNaN(date.valueOf())) return ''
  const offset = date.getTimezoneOffset() * 60_000
  return new Date(date.valueOf() - offset).toISOString().slice(0, 16)
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

function setOpen(next: boolean): void {
  if (!next) {
    form = keyForm()
    baseline.reset()
  }
  open = next
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
  if (saving) return
  if (!form.name.trim()) {
    toast.error(m.api_key_editor_api_key_name_required())
    return
  }
  if ((apiKey || form.customKey) && !form.key.trim()) {
    toast.error(m.api_key_editor_api_key_required())
    return
  }

  saving = true
  let savedExisting = false
  try {
    const input = {
      key: apiKey || form.customKey ? form.key.trim() : undefined,
      name: form.name.trim(),
      rpm_limit: form.rpmLimit === '' || form.rpmLimit == null ? null : Number(form.rpmLimit),
      mcp_access_enabled: form.mcpAccessEnabled,
      transparent_injection_enabled: form.transparentInjectionEnabled,
      inject_web_search: form.injectWebSearch,
      inject_media_understanding: form.injectMediaUnderstanding,
      inject_media_generation: form.injectMediaGeneration,
      expires_at: form.expiresAt ? new Date(form.expiresAt).toISOString() : undefined,
      model_ids: form.modelIds,
    }
    if (apiKey) {
      await admin.apiKeys.update(apiKey.id, { ...input, is_enabled: form.enabled })
      baseline.reset()
      if (presentation === 'sheet') open = false
      toast.success(m.api_key_editor_api_key_saved())
    } else {
      const created = await admin.apiKeys.create(input)
      createdSecret = created.key
      baseline.reset()
      toast.success(m.api_key_editor_api_key_created_copy_now_not_shown_again())
    }
    await queryClient.invalidateQueries({ queryKey: ['api-keys'] })
    savedExisting = apiKey != null
  } catch (error) {
    toast.error(localizeBackendErrorMessage(error))
  } finally {
    saving = false
  }
  // 解除保存锁之后再离开：保存中守卫会拦截路由跳转；基线已重置，此时跳转不再算作丢弃草稿。
  if (savedExisting) await onSaved?.()
}
</script>

{#snippet unavailableLink(href: string)}
  <a href={resolve(href)} class="text-sm text-muted-foreground underline underline-offset-2 hover:text-foreground"
    >{m.api_key_editor_platform_capability_disabled()}</a>
{/snippet}

{#snippet keyFields()}
  <Field.Group>
    <Field.Field size="name">
      <Field.Label for="api-key-name">{m.common_name()}</Field.Label>
      <Input id="api-key-name" class="max-w-(--field-control-width)" bind:value={form.name} required />
    </Field.Field>
    {#if !apiKey}
      <SettingRow
        title={m.api_key_editor_custom_api_key()}
        hint={m.api_key_editor_custom_api_key_help()}
        layout="inline"
        class="py-0">
        {#snippet children({ titleId })}
          <Switch id="api-key-custom" aria-labelledby={titleId} bind:checked={form.customKey} />
        {/snippet}
      </SettingRow>
    {/if}
    {#if apiKey || form.customKey}
      <Field.Field size="fill">
        <Field.Label for="api-key-secret" hint={m.api_key_editor_api_key_help()}
          >{m.api_key_editor_api_key()}</Field.Label>
        <div class="w-full max-w-(--field-control-width) min-w-0">
          <SecretInput
            id="api-key-secret"
            class="font-technical"
            bind:value={form.key}
            resetKey={secretResetKey}
            showLabel={m.api_key_editor_show_api_key()}
            hideLabel={m.api_key_editor_hide_api_key()}
            autocomplete="off"
            required>
            {#snippet actions()}
              <InputGroup.Button type="button" onclick={() => void copySecret(form.key)}>
                <CopyIcon data-icon="inline-start" />{m.common_copy()}
              </InputGroup.Button>
            {/snippet}
          </SecretInput>
        </div>
      </Field.Field>
    {/if}
    <Field.Field size="number">
      <Field.Label for="api-key-rpm-limit" hint={m.api_key_editor_rpm_limit_help()}>
        {m.api_key_editor_requests_per_minute()}
      </Field.Label>
      <InputGroup.Root class="max-w-(--field-control-width)">
        <InputGroup.Input
          id="api-key-rpm-limit"
          bind:value={form.rpmLimit}
          min="1"
          step="1"
          type="number"
          placeholder={m.api_key_editor_unlimited()} />
        <InputGroup.Addon align="inline-end">
          <InputGroup.Text class="font-technical text-xs">{m.api_key_editor_rpm_unit()}</InputGroup.Text>
        </InputGroup.Addon>
      </InputGroup.Root>
    </Field.Field>
    <Field.Field size="datetime">
      <Field.Label for="api-key-expires" hint={m.api_key_editor_never_expires_placeholder()}>
        {m.api_key_editor_expires()}
      </Field.Label>
      <Input
        id="api-key-expires"
        class="max-w-(--field-control-width)"
        bind:value={form.expiresAt}
        type="datetime-local" />
    </Field.Field>
  </Field.Group>
  <div class="mt-2 flex flex-col">
    {#if apiKey}
      <SettingRow title={m.common_enabled_status()} layout="inline">
        {#snippet children({ titleId })}
          <Switch id="api-key-enabled" aria-labelledby={titleId} bind:checked={form.enabled} />
        {/snippet}
      </SettingRow>
    {/if}
    <SettingRow
      title={m.api_key_editor_allow_all_models()}
      description={m.api_key_editor_unrestricted_model_scope_help()}
      layout="inline">
      {#snippet children({ titleId, descriptionId })}
        <Switch
          id="api-key-allow-all-models"
          aria-labelledby={titleId}
          aria-describedby={descriptionId}
          checked={allowAllModels}
          disabled={models.length === 0}
          onCheckedChange={setAllowAllModels} />
      {/snippet}
    </SettingRow>
    {#if !allowAllModels}
      <SettingRow title={m.api_key_editor_allowed_models()} controlSize="select" nested>
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
                      keywords={[effectiveModelDisplayName(model), model.model_id]}
                      data-checked
                      disabled={allowedModels.length === 1}
                      onSelect={() => setModelChecked(model.id, false)}>
                      <span class="min-w-0 flex-1 truncate">{effectiveModelDisplayName(model)}</span>
                      {#if logicalModelSecondaryId(model)}
                        <span class="truncate font-technical text-xs text-muted-foreground">{model.model_id}</span>
                      {/if}
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
                      keywords={[effectiveModelDisplayName(model), model.model_id]}
                      onSelect={() => setModelChecked(model.id, true)}>
                      <span class="min-w-0 flex-1 truncate">{effectiveModelDisplayName(model)}</span>
                      {#if logicalModelSecondaryId(model)}
                        <span class="truncate font-technical text-xs text-muted-foreground">{model.model_id}</span>
                      {/if}
                    </Command.Item>
                  {/each}
                </Command.Group>
              </Command.List>
            </Command.Root>
          </Popover.Content>
        </Popover.Root>
      </SettingRow>
    {/if}
    <SettingRow
      title={m.api_key_editor_allow_mcp_clients()}
      description={m.api_key_editor_lets_key_call_stravia_tools_mcp()}
      layout="inline">
      {#snippet children({ titleId, descriptionId })}
        <Switch
          id="api-key-mcp-access"
          aria-labelledby={titleId}
          aria-describedby={descriptionId}
          bind:checked={form.mcpAccessEnabled} />
      {/snippet}
    </SettingRow>
    <SettingRow
      title={m.api_key_editor_transparent_injection()}
      description={m.api_key_editor_transparent_injection_help()}
      layout="inline">
      {#snippet children({ titleId, descriptionId })}
        <Switch
          id="api-key-transparent-injection"
          aria-labelledby={titleId}
          aria-describedby={descriptionId}
          bind:checked={form.transparentInjectionEnabled} />
      {/snippet}
    </SettingRow>
    {#if form.transparentInjectionEnabled}
      <SettingRow title={m.api_key_editor_media_understanding()} layout="inline" nested>
        {#snippet note()}
          {#if !mediaUnderstandingEnabled}{@render unavailableLink('media-understanding')}{/if}
        {/snippet}
        {#snippet children({ titleId })}
          <Switch
            id="api-key-inject-media-understanding"
            aria-labelledby={titleId}
            bind:checked={form.injectMediaUnderstanding}
            disabled={!mediaUnderstandingEnabled} />
        {/snippet}
      </SettingRow>
      <SettingRow title={m.api_key_editor_media_generation()} layout="inline" nested>
        {#snippet note()}
          {#if mediaGenerationStatus === 'disabled'}
            {@render unavailableLink('media-generation')}
          {:else if mediaGenerationStatus === 'loading'}
            <span class="text-sm text-muted-foreground" role="status">{m.common_settings_loading()}</span>
          {:else if mediaGenerationStatus === 'unavailable'}
            <span class="text-sm text-destructive" role="status">
              {m.api_key_editor_platform_capability_unavailable()}
            </span>
          {/if}
        {/snippet}
        {#snippet children({ titleId })}
          <Switch
            id="api-key-inject-media-generation"
            aria-labelledby={titleId}
            bind:checked={form.injectMediaGeneration}
            disabled={mediaGenerationStatus !== 'enabled'}
            aria-busy={mediaGenerationStatus === 'loading'} />
        {/snippet}
      </SettingRow>
      <SettingRow title={m.api_key_editor_web_search()} layout="inline" nested>
        {#snippet note()}
          {#if !webSearchEnabled}{@render unavailableLink('web-search')}{/if}
        {/snippet}
        {#snippet children({ titleId })}
          <Switch
            id="api-key-inject-web-search"
            aria-labelledby={titleId}
            bind:checked={form.injectWebSearch}
            disabled={!webSearchEnabled} />
        {/snippet}
      </SettingRow>
    {/if}
  </div>
{/snippet}

{#snippet editorForm(pageMode: boolean)}
  <form
    class={pageMode ? 'content-column flex flex-1 flex-col gap-6' : 'route-overlay-form'}
    onsubmit={(event) => {
      event.preventDefault()
      void saveKey()
    }}>
    {#if pageMode}
      <!-- 保存中整组字段只读：fieldset[disabled] 同时锁定输入框、开关与选择器。 -->
      <fieldset class="m-0 flex min-w-0 flex-col gap-6 border-0 p-0" disabled={saving}>
        {@render keyFields()}
      </fieldset>
      <div
        class="sticky bottom-0 z-20 mt-auto flex translate-y-2 justify-end gap-2 border-t bg-background py-2 after:absolute after:inset-x-0 after:top-full after:h-2 after:bg-background after:content-['']">
        <Button href={resolve('api-keys')} variant="outline" disabled={saving}>{m.common_cancel()}</Button>

        <Button type="submit" disabled={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.api_key_editor_save_api_key()}
        </Button>
      </div>
    {:else}
      <div class="route-overlay-body">
        <fieldset class="m-0 flex min-w-0 flex-col gap-6 border-0 p-0" disabled={saving}>
          {@render keyFields()}
        </fieldset>
      </div>
      <Sheet.Footer class="route-overlay-footer"
        ><Button type="button" variant="outline" disabled={saving} onclick={() => guard.request(() => setOpen(false))}
          >{m.common_cancel()}</Button
        ><Button type="submit" disabled={saving}
          >{#if saving}<Spinner data-icon="inline-start" />{/if}{m.api_key_editor_save_api_key()}</Button
        ></Sheet.Footer>
    {/if}
  </form>
{/snippet}

{#if presentation === 'page'}
  <div class="route-page min-h-[calc(100svh-5rem)]">
    <PageHeader title={apiKey?.name.trim() || apiKey?.id || m.api_key_editor_edit_api_key()} />
    {@render editorForm(true)}
  </div>
{:else}
  <Sheet.Root bind:open={() => open, guard.openChange(setOpen)}>
    <Sheet.Content
      side="right"
      class="route-overlay-content w-full! gap-0 overflow-hidden p-0"
      locked={guard.busy}
      closeLabel={m.api_key_editor_close_api_key_editor()}>
      <Sheet.Header class="border-b">
        <Sheet.Title>{apiKey ? m.api_key_editor_edit_api_key() : m.common_create_api_key()}</Sheet.Title>
      </Sheet.Header>

      {#if createdSecret}
        <div class="route-overlay-body">
          <Field.Group>
            <Field.Field size="fill">
              <Field.Label for="created-api-key">{m.api_key_editor_copy_api_key_now()}</Field.Label>
              <SecretInput
                id="created-api-key"
                class="font-technical"
                value={createdSecret}
                resetKey={secretResetKey}
                showLabel={m.api_key_editor_show_api_key()}
                hideLabel={m.api_key_editor_hide_api_key()}
                readonly>
                {#snippet actions()}
                  <InputGroup.Button type="button" onclick={() => void copySecret(createdSecret ?? '')}>
                    <CopyIcon data-icon="inline-start" />{m.common_copy()}
                  </InputGroup.Button>
                {/snippet}
              </SecretInput>
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

<DraftDiscardDialog {guard} />
