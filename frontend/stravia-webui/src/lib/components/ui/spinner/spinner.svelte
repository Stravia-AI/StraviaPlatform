<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import Loader2Icon from '@lucide/svelte/icons/loader-2'
import { cn } from '$lib/utils.js'
import type { SVGAttributes } from 'svelte/elements'

let {
  class: className,
  role = 'status',
  // we add name, color, and stroke for compatibility with different icon libraries props
  name,
  color,
  stroke,
  'aria-label': ariaLabel,
  ...restProps
}: SVGAttributes<SVGSVGElement> = $props()
const accessibleLabel = $derived(ariaLabel ?? m.common_loading())
</script>

<Loader2Icon
  {role}
  name={name === null ? undefined : name}
  color={color === null ? undefined : color}
  stroke={stroke === null ? undefined : stroke}
  aria-label={accessibleLabel}
  class={cn('size-4 animate-spin', className)}
  {...restProps} />
