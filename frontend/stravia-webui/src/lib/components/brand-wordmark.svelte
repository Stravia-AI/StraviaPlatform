<script lang="ts">
import wordmarkMarkup from '../../assets/logos/stravia-wordmark.svg?raw'

import { cn } from '$lib/utils.js'

interface Props {
  class?: string
}

let { class: className }: Props = $props()
</script>

<span class={cn('inline-flex shrink-0 align-middle', className)} role="img" aria-label="Stravia">
  <!-- Fixed Sora 500 outlines from a checked-in asset; no runtime font or user content. -->
  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
  {@html wordmarkMarkup}
</span>
