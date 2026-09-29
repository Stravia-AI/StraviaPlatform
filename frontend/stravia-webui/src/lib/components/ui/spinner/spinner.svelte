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
// Lucide spreads rest props over the svg defaults, so a forwarded `undefined`
// would erase the built-in stroke. Only forward values the caller set.
const forwardedProps = $derived({
  ...(name != null ? { name } : {}),
  ...(color != null ? { color } : {}),
  ...(stroke != null ? { stroke } : {}),
})
</script>

<Loader2Icon
  {role}
  {...forwardedProps}
  aria-label={accessibleLabel}
  class={cn('size-4 animate-spin', className)}
  {...restProps} />
