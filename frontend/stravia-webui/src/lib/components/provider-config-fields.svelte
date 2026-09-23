<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { SvelteMap } from 'svelte/reactivity'
import { localeState } from '$lib/localization.svelte'
import { resolvePluginText } from '$lib/plugin-text'
import type { ProviderValidationIssue, VendorConfigField, LocalizedText } from '$lib/types'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import SecretInput from '$lib/components/secret-input.svelte'
import SecretTextarea from '$lib/components/secret-textarea.svelte'
import * as Select from '$lib/components/ui/select'
import { Switch } from '$lib/components/ui/switch'
import { Textarea } from '$lib/components/ui/textarea'

interface Props {
  fields: VendorConfigField[]
  configGroups?: Array<{ id: string; label: LocalizedText }>
  values?: Record<string, unknown>
  configuredSecretFields?: string[]
  satisfiedSecretFields?: string[]
  issues?: ProviderValidationIssue[]
  idPrefix: string
  onChanged?: () => void
}

let {
  fields,
  configGroups = [],
  values = $bindable({}),
  configuredSecretFields = [],
  satisfiedSecretFields = [],
  issues = [],
  idPrefix,
  onChanged,
}: Props = $props()

const availableSecretFields = $derived(new Set([...configuredSecretFields, ...satisfiedSecretFields]))
const groupLabels = $derived(new Map(configGroups.map((group) => [group.id, group.label])))
const visibleFields = $derived(
  fields.filter((field) => {
    const condition = field.visible_when
    if (!condition) return true
    if (!Object.prototype.hasOwnProperty.call(values, condition.field) && availableSecretFields.has(condition.field)) {
      return true
    }
    return Object.is(values[condition.field], condition.equals)
  }),
)
const unmatchedIssues = $derived(
  issues.filter((issue) => issue.field && !visibleFields.some((field) => field.key === issue.field)),
)
const groups = $derived.by(() => {
  const entries = new SvelteMap<string, VendorConfigField[]>()
  for (const field of visibleFields) {
    const group = field.group ?? ''
    const current = entries.get(group)
    if (current) current.push(field)
    else entries.set(group, [field])
  }
  return [...entries]
})

function fieldIssues(key: string): ProviderValidationIssue[] {
  return issues.filter((issue) => issue.field === key)
}

function fieldValue(field: VendorConfigField): unknown {
  if (Object.prototype.hasOwnProperty.call(values, field.key)) return values[field.key]
  if (field.secret && availableSecretFields.has(field.key)) return undefined
  return field.default_json ?? undefined
}

function stringValue(field: VendorConfigField): string {
  const value = fieldValue(field)
  return typeof value === 'string' || typeof value === 'number' ? String(value) : ''
}

function setValue(field: VendorConfigField, value: unknown): void {
  values = { ...values, [field.key]: value }
  onChanged?.()
}

function setText(field: VendorConfigField, value: string): void {
  if (field.kind.type === 'int' || field.kind.type === 'decimal') {
    if (value === '') {
      setValue(field, undefined)
      return
    }
    const parsed = Number(value)
    const valid = Number.isFinite(parsed) && (field.kind.type === 'decimal' || Number.isInteger(parsed))
    setValue(field, valid ? parsed : value)
    return
  }
  setValue(field, value)
}
</script>

{#each groups as [group, groupFields] (group)}
  <Field.Set class="sm:col-span-2">
    {#if group}<Field.Legend>{resolvePluginText(groupLabels.get(group)!, localeState.current)}</Field.Legend>{/if}
    <Field.Group class="grid gap-4 sm:grid-cols-2">
      {#each groupFields as field (field.key)}
        {@const controlId = `${idPrefix}-${field.key}`}
        {@const currentIssues = fieldIssues(field.key)}
        {@const invalid = currentIssues.length > 0}
        {#if field.kind.type === 'bool' && !field.secret}
          <Field.Field
            orientation="horizontal"
            class="min-h-10 justify-between rounded-lg border px-3 py-2 sm:col-span-2"
            data-invalid={invalid || undefined}>
            <div>
              <Field.Label
                for={controlId}
                hint={field.description ? resolvePluginText(field.description, localeState.current) : undefined}
                >{resolvePluginText(field.label, localeState.current)}</Field.Label>
              {#if field.required}<Field.Description>{m.provider_config_required()}</Field.Description>{/if}
            </div>
            <Switch
              id={controlId}
              checked={fieldValue(field) === true}
              aria-invalid={invalid || undefined}
              onCheckedChange={(checked: boolean) => setValue(field, checked)} />
            {#each currentIssues as issue, index (`${issue.field}:${issue.code}:${index}`)}
              <Field.Error>{resolvePluginText(issue.message, localeState.current)}</Field.Error>
            {/each}
          </Field.Field>
        {:else}
          <Field.Field size="fill" class="sm:col-span-2" data-invalid={invalid || undefined}>
            <Field.Label
              for={controlId}
              hint={field.description ? resolvePluginText(field.description, localeState.current) : undefined}
              >{resolvePluginText(field.label, localeState.current)}</Field.Label>
            {#if field.kind.type === 'bool'}
              <Select.Root
                type="single"
                value={typeof fieldValue(field) === 'boolean' ? String(fieldValue(field)) : ''}
                onValueChange={(value: string) => setValue(field, value === 'true')}>
                <Select.Trigger id={controlId} class="w-full" aria-invalid={invalid || undefined}>
                  {typeof fieldValue(field) === 'boolean'
                    ? fieldValue(field) === true
                      ? m.provider_config_enabled_value()
                      : m.provider_config_disabled_value()
                    : availableSecretFields.has(field.key)
                      ? m.provider_config_secret_configured_placeholder()
                      : m.provider_config_choose_value()}
                </Select.Trigger>
                <Select.Content>
                  <Select.Group>
                    <Select.Item value="true">{m.provider_config_enabled_value()}</Select.Item>
                    <Select.Item value="false">{m.provider_config_disabled_value()}</Select.Item>
                  </Select.Group>
                </Select.Content>
              </Select.Root>
            {:else if field.kind.type === 'enum'}
              {@const selectedEnumOption = field.kind.options.find((option) => option.value === stringValue(field))}
              <Select.Root
                type="single"
                value={stringValue(field)}
                onValueChange={(value: string) => setValue(field, value)}>
                <Select.Trigger id={controlId} class="w-full" aria-invalid={invalid || undefined}>
                  {field.secret && availableSecretFields.has(field.key) && !stringValue(field)
                    ? m.provider_config_secret_configured_placeholder()
                    : selectedEnumOption
                      ? resolvePluginText(selectedEnumOption.label, localeState.current)
                      : m.provider_config_choose_value()}
                </Select.Trigger>
                <Select.Content>
                  <Select.Group>
                    {#each field.kind.options as option (option.value)}
                      <Select.Item value={option.value}
                        >{resolvePluginText(option.label, localeState.current)}</Select.Item>
                    {/each}
                  </Select.Group>
                </Select.Content>
              </Select.Root>
            {:else if field.kind.type === 'string' && field.kind.multiline && field.secret}
              <SecretTextarea
                id={controlId}
                value={stringValue(field)}
                resetKey={`${idPrefix}:${field.key}`}
                required={field.required && !availableSecretFields.has(field.key)}
                maxlength={field.max_length ?? undefined}
                aria-invalid={invalid || undefined}
                autocomplete="off"
                placeholder={availableSecretFields.has(field.key)
                  ? m.provider_config_secret_configured_placeholder()
                  : undefined}
                oninput={(event: Event & { currentTarget: HTMLTextAreaElement }) =>
                  setText(field, event.currentTarget.value)} />
            {:else if field.kind.type === 'string' && field.kind.multiline}
              <Textarea
                id={controlId}
                class="min-h-28 font-technical"
                value={stringValue(field)}
                required={field.required}
                maxlength={field.max_length ?? undefined}
                aria-invalid={invalid || undefined}
                oninput={(event: Event & { currentTarget: HTMLTextAreaElement }) =>
                  setText(field, event.currentTarget.value)} />
            {:else if field.secret}
              <SecretInput
                id={controlId}
                class="font-technical"
                value={stringValue(field)}
                resetKey={`${idPrefix}:${field.key}`}
                required={field.required && !availableSecretFields.has(field.key)}
                maxlength={field.max_length ?? undefined}
                pattern={field.pattern ?? undefined}
                aria-invalid={invalid || undefined}
                autocomplete="off"
                placeholder={availableSecretFields.has(field.key)
                  ? m.provider_config_secret_configured_placeholder()
                  : undefined}
                oninput={(event: Event & { currentTarget: HTMLInputElement }) =>
                  setText(field, event.currentTarget.value)} />
            {:else}
              <Input
                id={controlId}
                class="font-technical"
                type={field.kind.type === 'int' || field.kind.type === 'decimal' ? 'number' : 'text'}
                value={stringValue(field)}
                required={field.required}
                min={field.min ?? undefined}
                max={field.max ?? undefined}
                step={field.kind.type === 'int' ? 1 : field.kind.type === 'decimal' ? 'any' : undefined}
                maxlength={field.max_length ?? undefined}
                pattern={field.pattern ?? undefined}
                aria-invalid={invalid || undefined}
                oninput={(event: Event & { currentTarget: HTMLInputElement }) =>
                  setText(field, event.currentTarget.value)} />
            {/if}
            {#each currentIssues as issue, index (`${issue.field}:${issue.code}:${index}`)}
              <Field.Error>{resolvePluginText(issue.message, localeState.current)}</Field.Error>
            {/each}
          </Field.Field>
        {/if}
      {/each}
    </Field.Group>
  </Field.Set>
{/each}

{#if unmatchedIssues.length > 0}
  <ul class="sm:col-span-2 flex list-disc flex-col gap-1 pl-5 text-sm text-destructive">
    {#each unmatchedIssues as issue, index (`${issue.field}:${issue.code}:${index}`)}
      <li>{resolvePluginText(issue.message, localeState.current)}</li>
    {/each}
  </ul>
{/if}
