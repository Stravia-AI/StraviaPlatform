<script lang="ts">
import { resolve } from '$app/paths'

import { cn } from '$lib/utils.js'

interface Props {
  class?: string
  label?: string
}

let { class: className, label = 'Stravia 观策行' }: Props = $props()

const primarySource = resolve('/stravia-logo.svg')
const reversedSource = resolve('/stravia-logo-reversed.svg')
</script>

<span class={cn('relative inline-grid shrink-0', className)} role="img" aria-label={label}>
  <img class="size-full object-contain dark:hidden" src={primarySource} alt="" decoding="async" draggable="false" />
  <img
    class="hidden size-full object-contain dark:block"
    src={reversedSource}
    alt=""
    decoding="async"
    draggable="false" />
</span>
