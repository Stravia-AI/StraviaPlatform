<script lang="ts">
import CircleHelpIcon from '@lucide/svelte/icons/circle-help'

import * as m from '#lib/paraglide/messages.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'

/** 标题或 label 右侧的 16px 问号提示：可选解释在任何宽度都只在这里出现（hover、键盘 focus 或点击/触摸）。 */
let { hint }: { hint: string } = $props()

const hintId = $props.id()
let open = $state(false)
</script>

<Tooltip.Root bind:open delayDuration={0} disableCloseOnTriggerClick>
  <Tooltip.Trigger
    type="button"
    data-slot="field-hint"
    class="relative inline-flex size-4 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-[color,transform] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:text-foreground"
    aria-label={m.common_field_help()}
    aria-describedby={hintId}
    onclick={() => (open = true)}>
    <CircleHelpIcon class="size-4" />
  </Tooltip.Trigger>
  <Tooltip.Content id={hintId} class="max-w-80 text-pretty">
    {hint}
  </Tooltip.Content>
</Tooltip.Root>
