<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import {
  formatSpecificationTokens,
  specificationModalities,
  specificationReasoningEfforts,
} from '$lib/model-specification'
import type { SpecificationFilter } from '$lib/model-specification-filter'
import { Checkbox } from '$lib/components/ui/checkbox'
import * as Field from '$lib/components/ui/field'
import { Input } from '$lib/components/ui/input'
import * as Select from '$lib/components/ui/select'

interface Props {
  value: SpecificationFilter
  onChange: (value: SpecificationFilter) => void
}

let { value, onChange }: Props = $props()
const id = $props.id()
const limits = $derived([
  {
    key: 'context' as const,
    label: m.model_specification_minimum_context(),
    presetLabel: m.model_specification_context_preset(),
    presets: [32000, 128000, 200000, 1000000],
  },
])

function setLimit(key: 'context', input: HTMLInputElement): void {
  const number = input.valueAsNumber
  const valid = input.value === '' || (Number.isSafeInteger(number) && number >= 0)
  if (valid) onChange({ ...value, [key]: input.value === '' ? undefined : number })
  else input.reportValidity()
}

function toggle<T extends string>(selected: T[], key: T, checked: boolean): T[] {
  return checked ? [...selected, key] : selected.filter((entry) => entry !== key)
}
</script>

<Field.FieldDescription>{m.model_specification_match_all()}</Field.FieldDescription>
<Field.FieldGroup>
  {#each limits as limit (limit.key)}
    {@const selected = value[limit.key]}
    <Field.Field>
      <Field.FieldLabel for={`${id}-${limit.key}`}>{limit.label}</Field.FieldLabel>
      <Select.Root
        type="single"
        value={selected == null ? 'none' : limit.presets.includes(selected) ? String(selected) : 'custom'}
        onValueChange={(next) => {
          if (next === 'custom') document.getElementById(`${id}-${limit.key}`)?.focus()
          else onChange({ ...value, [limit.key]: next === 'none' ? undefined : Number(next) })
        }}>
        <Select.Trigger role="combobox" aria-label={limit.presetLabel} class="w-full">
          {selected == null
            ? m.model_specification_no_minimum()
            : limit.presets.includes(selected)
              ? formatSpecificationTokens(selected)
              : m.model_specification_custom()}
        </Select.Trigger>
        <Select.Content>
          <Select.Group>
            <Select.Item value="none">{m.model_specification_no_minimum()}</Select.Item>
            {#each limit.presets as preset (preset)}
              <Select.Item value={String(preset)}>{formatSpecificationTokens(preset)}</Select.Item>
            {/each}
            <Select.Item value="custom">{m.model_specification_custom()}</Select.Item>
          </Select.Group>
        </Select.Content>
      </Select.Root>
      <Input
        id={`${id}-${limit.key}`}
        type="number"
        min="0"
        max={Number.MAX_SAFE_INTEGER}
        step="1"
        value={selected ?? ''}
        placeholder={m.model_specification_no_minimum()}
        oninput={(event: Event) => setLimit(limit.key, event.currentTarget as HTMLInputElement)} />
      <Field.FieldDescription>{m.model_specification_integer_required()}</Field.FieldDescription>
    </Field.Field>
  {/each}
  {#each ['inputModalities', 'outputModalities'] as direction (direction)}
    {@const key = direction as 'inputModalities' | 'outputModalities'}
    <Field.FieldSet>
      <Field.FieldLegend
        >{key === 'inputModalities'
          ? m.model_specification_input_modalities()
          : m.model_specification_output_modalities()}</Field.FieldLegend>
      <Field.FieldGroup class="grid grid-cols-2 gap-2">
        {#each specificationModalities as modality (modality.key)}
          <Field.Field orientation="horizontal">
            <Checkbox
              id={`${id}-${key}-${modality.key}`}
              checked={value[key].includes(modality.key)}
              onCheckedChange={(checked) =>
                onChange({ ...value, [key]: toggle(value[key], modality.key, checked === true) })} />
            <Field.FieldLabel for={`${id}-${key}-${modality.key}`} class="gap-1.5"
              ><modality.icon
                class="size-3.5 shrink-0 text-muted-foreground"
                aria-hidden="true" />{modality.label()}</Field.FieldLabel>
          </Field.Field>
        {/each}
      </Field.FieldGroup>
    </Field.FieldSet>
  {/each}
  <Field.FieldSet>
    <Field.FieldLegend>{m.model_specification_reasoning_efforts()}</Field.FieldLegend>
    <Field.FieldGroup class="grid grid-cols-2 gap-2">
      {#each [...new Set([...specificationReasoningEfforts, ...value.reasoningEfforts])] as effort (effort)}
        <Field.Field orientation="horizontal">
          <Checkbox
            id={`${id}-effort-${effort}`}
            checked={value.reasoningEfforts.includes(effort)}
            onCheckedChange={(checked) =>
              onChange({ ...value, reasoningEfforts: toggle(value.reasoningEfforts, effort, checked === true) })} />
          <Field.FieldLabel for={`${id}-effort-${effort}`} class="font-technical">{effort}</Field.FieldLabel>
        </Field.Field>
      {/each}
    </Field.FieldGroup>
    <Field.FieldLabel for={`${id}-custom-effort`}>{m.model_specification_custom_effort()}</Field.FieldLabel>
    <Input
      id={`${id}-custom-effort`}
      class="font-technical"
      onchange={(event: Event) => {
        const input = event.currentTarget as HTMLInputElement
        const effort = input.value.trim()
        if (effort && !['default', 'null'].includes(effort.toLowerCase()) && !value.reasoningEfforts.includes(effort))
          onChange({ ...value, reasoningEfforts: [...value.reasoningEfforts, effort] })
        input.value = ''
      }} />
  </Field.FieldSet>
</Field.FieldGroup>
