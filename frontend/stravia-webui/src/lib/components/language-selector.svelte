<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { localeState } from '$lib/localization.svelte'
import * as Field from '$lib/components/ui/field'
import * as Select from '$lib/components/ui/select'

interface Props {
  class?: string
  description?: boolean
}

let { class: className, description = true }: Props = $props()

function setSelectedLocale(value: string): void {
  if (value === 'en-US' || value === 'zh-CN') localeState.set(value)
}
</script>

<Field.Field class={className} size="select">
  <Field.FieldLabel for="interface-language" hint={description ? m.locale_language_description() : undefined}>
    {m.locale_language_label()}
  </Field.FieldLabel>
  <Select.Root
    type="single"
    value={localeState.current}
    onValueChange={setSelectedLocale}>
    <Select.Trigger id="interface-language" class="w-full" aria-label={m.locale_language_label()}>
      {localeState.current === 'zh-CN' ? m.locale_simplified_chinese_autonym() : m.locale_english_autonym()}
    </Select.Trigger>
    <Select.Content>
      <Select.Group>
        <Select.Item value="en-US" label={m.locale_english_autonym()}>{m.locale_english_autonym()}</Select.Item>
        <Select.Item value="zh-CN" label={m.locale_simplified_chinese_autonym()}>
          {m.locale_simplified_chinese_autonym()}
        </Select.Item>
      </Select.Group>
    </Select.Content>
  </Select.Root>
</Field.Field>
