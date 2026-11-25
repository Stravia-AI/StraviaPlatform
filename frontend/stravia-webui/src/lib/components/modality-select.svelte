<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { specificationModalities, specificationModality } from '#lib/model-specification.js'
import * as Select from '#lib/components/ui/select/index.js'

interface Props {
  id: string
  target: 'input' | 'output'
  value: string[]
  disabled?: boolean
  registered?: readonly string[]
}

let { id, target, value = $bindable(), disabled = false, registered = [] }: Props = $props()

// 已登记的自定义模态在取消选中后仍可重新选择，直到编辑完成。
const options = $derived([...new Set([...specificationModalities.map(({ key }) => key), ...registered, ...value])])

function setValue(selected: string[]): void {
  const picked = new Set(selected)
  // 按固定顺序保存，避免点选顺序不同产生无意义的规格差异。
  value = options.filter((option) => picked.has(option))
}
</script>

<!-- 模型详情与表格单元格编辑共用：触发器与选项都以「图标 + 名称」呈现同一种模态。 -->
<Select.Root type="multiple" {disabled} bind:value={() => value, setValue}>
  <Select.Trigger {id} class="w-full min-w-0" data-modality-select={target}>
    {#if value.length > 0}
      <span role="list" class="flex min-w-0 items-center gap-3 overflow-hidden">
        {#each value as item (item)}
          {@const modality = specificationModality(item)}
          <span role="listitem" class="inline-flex shrink-0 items-center gap-1.5">
            <modality.icon class="size-3.5 text-muted-foreground" aria-hidden="true" />{modality.label()}
          </span>
        {/each}
      </span>
    {:else}
      <span class="truncate text-muted-foreground">{m.provider_model_editor_select_content_types()}</span>
    {/if}
  </Select.Trigger>
  <Select.Content>
    <Select.Group>
      {#each options as option (option)}
        {@const modality = specificationModality(option)}
        <Select.Item value={option} label={modality.label()}>
          <modality.icon class="size-3.5 text-muted-foreground" aria-hidden="true" />{modality.label()}
        </Select.Item>
      {/each}
    </Select.Group>
  </Select.Content>
</Select.Root>
