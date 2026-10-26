<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import PlusIcon from '@lucide/svelte/icons/plus'

import { specificationReasoningEfforts } from '#lib/model-specification.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Command from '#lib/components/ui/command/index.js'
import * as Popover from '#lib/components/ui/popover/index.js'

interface Props {
  id: string
  value: string[]
  disabled?: boolean
  registered?: readonly string[]
}

let { id, value = $bindable(), disabled = false, registered = [] }: Props = $props()
let open = $state(false)
let query = $state('')

// 已登记的自定义强度在取消选中后仍可重新选择，直到编辑完成。
const options = $derived([...new Set([...specificationReasoningEfforts, ...registered, ...value])])
const needle = $derived(query.trim())
const matches = $derived(options.filter((option) => option.toLowerCase().includes(needle.toLowerCase())))
// default / null 在请求里表示“沿用上游默认”，不能登记为一个强度。
const creatable = $derived(
  needle !== '' && !['default', 'null'].includes(needle.toLowerCase()) && !options.includes(needle),
)

function toggle(option: string): void {
  const picked = value.includes(option) ? value.filter((item) => item !== option) : [...value, option]
  // 按固定顺序保存，避免点选顺序不同产生无意义的规格差异。
  value = options.filter((item) => picked.includes(item))
}

function addCustom(): void {
  if (!creatable) return
  value = [...value, needle]
  query = ''
}
</script>

<!-- 模型详情与表格单元格编辑共用：下拉里既能勾选已知强度，也能直接输入并添加自定义强度。 -->
<Popover.Root
  bind:open
  onOpenChange={(next: boolean) => {
    if (!next) query = ''
  }}>
  <Popover.Trigger>
    {#snippet child({ props })}
      <Button
        {...props}
        {id}
        type="button"
        variant="outline"
        class="w-full min-w-0 justify-between font-normal"
        role="combobox"
        aria-expanded={open}
        aria-haspopup="listbox"
        data-effort-values-select
        {disabled}>
        {#if value.length > 0}
          <span class="truncate font-technical">{value.join(', ')}</span>
        {:else}
          <span class="truncate text-muted-foreground">{m.provider_model_editor_select_efforts()}</span>
        {/if}
        <ChevronDownIcon data-icon="inline-end" class="text-muted-foreground" />
      </Button>
    {/snippet}
  </Popover.Trigger>
  <Popover.Content align="start" class="w-(--bits-popover-anchor-width) min-w-56 max-w-[calc(100vw-2rem)] p-0">
    <!-- Command 把 label 作为搜索框的可访问名称，所以这里必须是搜索框自己的用途，而不是字段名。 -->
    <Command.Root shouldFilter={false} label={m.provider_model_editor_effort_search()}>
      <Command.Input bind:value={query} placeholder={m.provider_model_editor_effort_search()} />
      <Command.List>
        {#if matches.length === 0 && !creatable}
          <Command.Empty>{m.provider_model_editor_effort_no_match()}</Command.Empty>
        {/if}
        {#each matches as option (option)}
          <Command.Item
            value={option}
            data-checked={value.includes(option)}
            aria-checked={value.includes(option)}
            onSelect={() => toggle(option)}>
            <span class="truncate font-technical">{option}</span>
          </Command.Item>
        {/each}
        {#if creatable}
          <Command.Item value={`custom:${needle}`} onSelect={addCustom}>
            <PlusIcon class="size-4 shrink-0" aria-hidden="true" />
            <span class="truncate">{m.provider_model_editor_effort_add_custom({ value: needle })}</span>
          </Command.Item>
        {/if}
      </Command.List>
    </Command.Root>
  </Popover.Content>
</Popover.Root>
