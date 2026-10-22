<script lang="ts">
import GlobeIcon from '@lucide/svelte/icons/globe'

import * as m from '#lib/paraglide/messages.js'
import { localeState } from '#lib/localization.svelte.js'
import * as Field from '#lib/components/ui/field/index.js'
import * as Select from '#lib/components/ui/select/index.js'

interface Props {
  class?: string
  description?: boolean
  /** 独立页面使用：不带可见 label 的紧凑控件，名称由 aria-label 提供。 */
  compact?: boolean
}

let { class: className, description = true, compact = false }: Props = $props()

function setSelectedLocale(value: string): void {
  if (value === 'en-US' || value === 'zh-CN') localeState.set(value)
}
</script>

{#snippet languageSelect(triggerClass: string)}
  <Select.Root type="single" value={localeState.current} onValueChange={setSelectedLocale}>
    <Select.Trigger id="interface-language" class={triggerClass} aria-label={m.locale_language_label()}>
      {#if compact}<GlobeIcon class="size-4 text-muted-foreground" aria-hidden="true" />{/if}
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
{/snippet}

{#if compact}
  <div class={className}>{@render languageSelect('w-36 max-w-full')}</div>
{:else}
  <Field.Field class={className} size="setting">
    <Field.FieldLabel for="interface-language" hint={description ? m.locale_language_description() : undefined}>
      {m.locale_language_label()}
    </Field.FieldLabel>
    {@render languageSelect('w-full')}
  </Field.Field>
{/if}
