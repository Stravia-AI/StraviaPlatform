<script lang="ts">
import CircleHelpIcon from '@lucide/svelte/icons/circle-help'
import type { ComponentProps } from 'svelte'

import * as m from '$lib/paraglide/messages.js'
import { Label } from '$lib/components/ui/label/index.js'
import * as Tooltip from '$lib/components/ui/tooltip'
import { cn } from '$lib/utils.js'

let {
  ref = $bindable(null),
  class: className,
  children,
  hint,
  ...restProps
}: ComponentProps<typeof Label> & { hint?: string } = $props()

const hintId = $props.id()
</script>

<div
  data-slot="field-label"
  data-has-hint={hint ? 'true' : undefined}
  class="group/field-label min-w-0">
  <div class="flex w-fit min-w-0 items-center gap-2">
    <Label
      bind:ref
      class={cn(
        'leading-snug group-data-[disabled=true]/field:opacity-50 has-data-checked:border-primary/30 has-data-checked:bg-primary/5 has-[>[data-slot=field]]:rounded-lg has-[>[data-slot=field]]:border *:data-[slot=field]:p-2.5 dark:has-data-checked:border-primary/20 dark:has-data-checked:bg-primary/10 peer/field-label w-fit leading-snug has-[>[data-slot=field]]:w-full has-[>[data-slot=field]]:flex-col',
        className,
      )}
      {...restProps}>
      {@render children?.()}
    </Label>
    {#if hint}
      <Tooltip.Root delayDuration={0}>
        <Tooltip.Trigger
          type="button"
          data-slot="field-hint"
          class="relative -m-2 inline-flex size-10 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-[color,transform] duration-[140ms] ease-[cubic-bezier(0.2,0,0,1)] hover:text-foreground"
          aria-label={m.common_field_help()}
          aria-describedby={hintId}>
          <CircleHelpIcon class="size-4" />
        </Tooltip.Trigger>
        <Tooltip.Content id={hintId} class="max-w-80 text-pretty">
          {hint}
        </Tooltip.Content>
      </Tooltip.Root>
    {/if}
  </div>
  {#if hint}
    <p data-slot="field-hint-text" class="hidden max-w-2xl text-[0.8125rem] leading-[1.45] text-muted-foreground">
      {hint}
    </p>
  {/if}
</div>
