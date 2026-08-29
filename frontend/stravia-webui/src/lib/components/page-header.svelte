<script lang="ts">
import type { Snippet } from 'svelte'

interface Props {
  eyebrow: string
  title: string
  description: string
  actions?: Snippet
  meta?: Snippet
}

let { eyebrow, title, description, actions, meta }: Props = $props()
</script>

<header class="route-page-header">
  <div class="min-w-0">
    <div class="mb-2 flex flex-wrap items-center gap-x-3 gap-y-1">
      <p class="font-structural text-[0.72rem] font-semibold tracking-[0.14em] text-primary uppercase">{eyebrow}</p>
      {#if meta}{@render meta()}{/if}
    </div>
    <h1
      class="font-structural text-[1.625rem] leading-[1.15] font-semibold tracking-[-0.025em] text-balance sm:text-[1.875rem]">
      {title}
    </h1>
    <p class="mt-2 max-w-3xl text-sm leading-[1.5] text-pretty text-muted-foreground">{description}</p>
  </div>
  {#if actions}
    <div class="flex shrink-0 flex-wrap items-center gap-2">{@render actions()}</div>
  {/if}
</header>
