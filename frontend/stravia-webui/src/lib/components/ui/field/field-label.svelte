<script lang="ts">
import type { Label as LabelPrimitive } from 'bits-ui'

import { Label } from '#lib/components/ui/label/index.js'
import FieldHelp from './field-help.svelte'
import { cn } from '#lib/utils.js'

let {
  ref = $bindable(null),
  class: className,
  children,
  hint,
  ...restProps
}: LabelPrimitive.RootProps & { hint?: string } = $props()
</script>

<div data-slot="field-label" class="group/field-label min-w-0">
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
    {#if hint}<FieldHelp {hint} />{/if}
  </div>
</div>
