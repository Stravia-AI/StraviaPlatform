<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { toast } from 'svelte-sonner'
import TypeIcon from '@lucide/svelte/icons/case-sensitive'
import RegexIcon from '@lucide/svelte/icons/regex'
import TrashIcon from '@lucide/svelte/icons/trash-2'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import {
  customRuleFieldError,
  draftFromRule,
  draftToInput,
  emptyDraft,
  isFieldError,
  type CustomRuleField,
  type CustomRuleFieldError,
} from '$lib/credential-custom-rule'
import type { CustomCredentialRule } from '$lib/types'
import SecretTextarea from '$lib/components/secret-textarea.svelte'
import * as Accordion from '$lib/components/ui/accordion'
import * as Alert from '$lib/components/ui/alert'
import * as AlertDialog from '$lib/components/ui/alert-dialog'
import { Button } from '$lib/components/ui/button'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Sheet from '$lib/components/ui/sheet'
import { Spinner } from '$lib/components/ui/spinner'
import { Switch } from '$lib/components/ui/switch'
import * as Tabs from '$lib/components/ui/tabs'

let { onchanged }: { onchanged: () => void } = $props()

let open = $state(false)
let rule = $state.raw<CustomCredentialRule | null>(null)
let draft = $state(emptyDraft())
let fieldError = $state.raw<CustomRuleFieldError | null>(null)
let formError = $state('')
let saving = $state(false)
let deleteOpen = $state(false)
let deleting = $state(false)
let advanced = $state('')
// 每次打开都重新遮蔽敏感文本。
let session = $state(0)
const busy = $derived(saving || deleting)

/** 传入规则进入编辑；传 null 新建。草稿在打开时一次性建立，关闭后丢弃。 */
export function show(target: CustomCredentialRule | null): void {
  rule = target
  draft = target ? draftFromRule(target) : emptyDraft()
  advanced = draft.minEntropy ? 'advanced' : ''
  fieldError = null
  formError = ''
  session += 1
  open = true
}

function errorFor(field: CustomRuleField): string | undefined {
  return fieldError?.field === field ? fieldError.message : undefined
}

async function save(): Promise<void> {
  if (busy) return
  const input = draftToInput(draft)
  if (isFieldError(input)) {
    fieldError = input
    return
  }
  saving = true
  fieldError = null
  formError = ''
  try {
    if (rule) await admin.credentialProtection.customRules.update(rule.id, input)
    else await admin.credentialProtection.customRules.create(input)
    toast.success(m.credential_custom_saved())
    open = false
    onchanged()
  } catch (error) {
    fieldError = customRuleFieldError(error)
    if (!fieldError) formError = localizeBackendErrorMessage(error)
  } finally {
    saving = false
  }
}

async function remove(): Promise<void> {
  if (!rule || busy) return
  deleting = true
  formError = ''
  try {
    await admin.credentialProtection.customRules.delete(rule.id)
    toast.success(m.credential_custom_deleted())
    deleteOpen = false
    open = false
    onchanged()
  } catch (error) {
    deleteOpen = false
    formError = localizeBackendErrorMessage(error)
  } finally {
    deleting = false
  }
}
</script>

<Sheet.Root bind:open>
  <Sheet.Content closeLabel={m.common_close()} class="gap-0 data-[side=right]:w-full data-[side=right]:sm:max-w-xl">
    <Sheet.Header class="border-b pr-12">
      <Sheet.Title>{rule ? m.credential_custom_edit() : m.credential_custom_add()}</Sheet.Title>
      <Sheet.Description>{m.credential_custom_sheet_description()}</Sheet.Description>
    </Sheet.Header>
    <form
      class="flex min-h-0 flex-1 flex-col"
      autocomplete="off"
      onsubmit={(event) => {
        event.preventDefault()
        void save()
      }}>
      <div class="flex-1 overflow-y-auto px-4 py-5">
        <Field.Group>
          <Field.Field orientation="vertical" data-invalid={!!errorFor('name')}>
            <Field.Label for="custom-rule-name">{m.credential_protection_rule_name()}</Field.Label>
            <Input
              id="custom-rule-name"
              bind:value={draft.name}
              placeholder={m.credential_custom_name_placeholder()}
              disabled={busy}
              aria-invalid={!!errorFor('name')}
              aria-describedby={errorFor('name') ? 'custom-rule-name-error' : undefined} />
            <Field.Error id="custom-rule-name-error">{errorFor('name')}</Field.Error>
          </Field.Field>

          <Field.Field orientation="vertical">
            <Field.Label id="custom-rule-mode-label">{m.credential_custom_mode()}</Field.Label>
            <Tabs.Root bind:value={draft.mode} class="gap-4">
              <Tabs.List aria-labelledby="custom-rule-mode-label">
                <Tabs.Trigger value="simple" disabled={busy}
                  ><TypeIcon />{m.credential_custom_mode_simple()}</Tabs.Trigger>
                <Tabs.Trigger value="pattern" disabled={busy}
                  ><RegexIcon />{m.credential_custom_mode_pattern()}</Tabs.Trigger>
              </Tabs.List>

              <Tabs.Content value="simple" class="flex flex-col gap-4">
                <p class="text-sm text-muted-foreground">{m.credential_custom_mode_simple_help()}</p>
                <Field.Field orientation="vertical" data-invalid={!!errorFor('text')}>
                  <Field.Label for="custom-rule-text">{m.credential_custom_text()}</Field.Label>
                  <SecretTextarea
                    id="custom-rule-text"
                    bind:value={draft.text}
                    resetKey={session}
                    rows={4}
                    spellcheck={false}
                    autocapitalize="off"
                    disabled={busy}
                    aria-invalid={!!errorFor('text')}
                    aria-describedby="custom-rule-text-help custom-rule-text-error" />
                  <Field.Description id="custom-rule-text-help">{m.credential_custom_text_help()}</Field.Description>
                  <Field.Error id="custom-rule-text-error">{errorFor('text')}</Field.Error>
                </Field.Field>
              </Tabs.Content>

              <Tabs.Content value="pattern" class="flex flex-col gap-4">
                <p class="text-sm text-muted-foreground">{m.credential_custom_mode_pattern_help()}</p>
                <Field.Field orientation="vertical" data-invalid={!!errorFor('regex')}>
                  <Field.Label for="custom-rule-regex">{m.credential_protection_rule_expression()}</Field.Label>
                  <Input
                    id="custom-rule-regex"
                    class="font-technical"
                    bind:value={draft.regex}
                    spellcheck={false}
                    autocapitalize="off"
                    disabled={busy}
                    aria-invalid={!!errorFor('regex')}
                    aria-describedby="custom-rule-regex-help custom-rule-regex-error" />
                  <Field.Description id="custom-rule-regex-help">{m.credential_custom_regex_help()}</Field.Description>
                  <Field.Error id="custom-rule-regex-error">{errorFor('regex')}</Field.Error>
                </Field.Field>
                <Field.Field orientation="vertical" data-invalid={!!errorFor('secret_group')}>
                  <Field.Label for="custom-rule-group">{m.credential_custom_group()}</Field.Label>
                  <Input
                    id="custom-rule-group"
                    class="font-technical w-28"
                    inputmode="numeric"
                    bind:value={draft.secretGroup}
                    disabled={busy}
                    aria-invalid={!!errorFor('secret_group')}
                    aria-describedby="custom-rule-group-help custom-rule-group-error" />
                  <Field.Description id="custom-rule-group-help">{m.credential_custom_group_help()}</Field.Description>
                  <Field.Error id="custom-rule-group-error">{errorFor('secret_group')}</Field.Error>
                </Field.Field>
                <Field.Field orientation="vertical" data-invalid={!!errorFor('keywords')}>
                  <Field.Label for="custom-rule-keywords">{m.credential_protection_keywords()}</Field.Label>
                  <Input
                    id="custom-rule-keywords"
                    class="font-technical"
                    bind:value={draft.keywords}
                    placeholder={m.credential_custom_keywords_placeholder()}
                    spellcheck={false}
                    autocapitalize="off"
                    disabled={busy}
                    aria-invalid={!!errorFor('keywords')}
                    aria-describedby="custom-rule-keywords-help custom-rule-keywords-error" />
                  <Field.Description id="custom-rule-keywords-help"
                    >{m.credential_custom_keywords_help()}</Field.Description>
                  <Field.Error id="custom-rule-keywords-error">{errorFor('keywords')}</Field.Error>
                </Field.Field>
                <Accordion.Root type="single" bind:value={advanced}>
                  <Accordion.Item value="advanced">
                    <Accordion.Trigger>{m.common_advanced()}</Accordion.Trigger>
                    <Accordion.Content>
                      <Field.Field orientation="vertical" data-invalid={!!errorFor('min_entropy')}>
                        <Field.Label for="custom-rule-entropy">{m.credential_custom_min_entropy()}</Field.Label>
                        <Input
                          id="custom-rule-entropy"
                          class="font-technical w-28"
                          inputmode="decimal"
                          bind:value={draft.minEntropy}
                          disabled={busy}
                          aria-invalid={!!errorFor('min_entropy')}
                          aria-describedby="custom-rule-entropy-help custom-rule-entropy-error" />
                        <Field.Description id="custom-rule-entropy-help"
                          >{m.credential_custom_min_entropy_help()}</Field.Description>
                        <Field.Error id="custom-rule-entropy-error">{errorFor('min_entropy')}</Field.Error>
                      </Field.Field>
                    </Accordion.Content>
                  </Accordion.Item>
                </Accordion.Root>
              </Tabs.Content>
            </Tabs.Root>
          </Field.Field>

          <Field.Field orientation="vertical" data-invalid={!!errorFor('description')}>
            <Field.Label for="custom-rule-description">{m.credential_custom_description()}</Field.Label>
            <Input
              id="custom-rule-description"
              bind:value={draft.description}
              disabled={busy}
              aria-invalid={!!errorFor('description')}
              aria-describedby="custom-rule-description-help custom-rule-description-error" />
            <Field.Description id="custom-rule-description-help"
              >{m.credential_custom_description_help()}</Field.Description>
            <Field.Error id="custom-rule-description-error">{errorFor('description')}</Field.Error>
          </Field.Field>

          <Field.Field orientation="horizontal">
            <Field.Content>
              <Field.Label for="custom-rule-enabled">{m.credential_custom_enabled()}</Field.Label>
              <Field.Description>{m.credential_custom_enabled_help()}</Field.Description>
            </Field.Content>
            <Switch id="custom-rule-enabled" bind:checked={draft.enabled} disabled={busy} />
          </Field.Field>

          {#if formError}
            <Alert.Root variant="destructive"><Alert.Description>{formError}</Alert.Description></Alert.Root>
          {/if}
        </Field.Group>
      </div>
      <Sheet.Footer class="border-t">
        {#if rule}
          <Button
            type="button"
            variant="outline"
            class="mr-auto text-destructive hover:text-destructive"
            disabled={busy}
            onclick={() => {
              deleteOpen = true
            }}><TrashIcon data-icon="inline-start" />{m.credential_custom_delete()}</Button>
        {/if}
        <Button
          type="button"
          variant="outline"
          disabled={busy}
          onclick={() => {
            open = false
          }}>{m.common_cancel()}</Button>
        <Button type="submit" disabled={busy} aria-busy={saving}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{rule
            ? m.credential_custom_save()
            : m.credential_custom_create()}
        </Button>
      </Sheet.Footer>
    </form>
  </Sheet.Content>
</Sheet.Root>

<AlertDialog.Root bind:open={deleteOpen}>
  <AlertDialog.Content>
    <AlertDialog.Header>
      <AlertDialog.Title>{m.credential_custom_delete_title({ name: rule?.name ?? '' })}</AlertDialog.Title>
      <AlertDialog.Description>{m.credential_custom_delete_description()}</AlertDialog.Description>
    </AlertDialog.Header>
    <AlertDialog.Footer>
      <AlertDialog.Cancel disabled={deleting}>{m.common_cancel()}</AlertDialog.Cancel>
      <AlertDialog.Action variant="destructive" disabled={deleting} onclick={() => void remove()}
        >{m.credential_custom_delete()}</AlertDialog.Action>
    </AlertDialog.Footer>
  </AlertDialog.Content>
</AlertDialog.Root>
