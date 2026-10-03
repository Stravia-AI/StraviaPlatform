<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { specificationModality } from '$lib/model-specification'
import { cn } from '$lib/utils'
import * as Tooltip from '$lib/components/ui/tooltip'

interface Props {
  values: readonly string[] | undefined
  /** 位于按钮等交互元素内部时关闭 tooltip，避免嵌套交互；名称仍由读屏文本提供。 */
  tooltip?: boolean
  class?: string
}

let { values, tooltip = true, class: className }: Props = $props()
</script>

{#if values?.length}
  <span role="list" class={cn('inline-flex min-w-0 flex-wrap items-center gap-0.5', className)}>
    {#each values as value (value)}
      {@const modality = specificationModality(value)}
      {#if tooltip}
        <Tooltip.Root>
          <Tooltip.Trigger>
            {#snippet child({ props })}
              <!-- 只承载补充说明，不增加 Tab 停留；读屏通过 sr-only 文本获得名称 -->
              <span {...props} tabindex="-1" role="listitem" class="inline-flex size-5 items-center justify-center">
                <modality.icon class="size-3.5" aria-hidden="true" />
                <span class="sr-only">{modality.label()}</span>
              </span>
            {/snippet}
          </Tooltip.Trigger>
          <Tooltip.Content>{modality.label()}</Tooltip.Content>
        </Tooltip.Root>
      {:else}
        <span role="listitem" class="inline-flex size-5 items-center justify-center" title={modality.label()}>
          <modality.icon class="size-3.5" aria-hidden="true" />
          <span class="sr-only">{modality.label()}</span>
        </span>
      {/if}
    {/each}
  </span>
{:else}
  <span class={cn('text-muted-foreground', className)}>{m.model_specification_not_registered()}</span>
{/if}
