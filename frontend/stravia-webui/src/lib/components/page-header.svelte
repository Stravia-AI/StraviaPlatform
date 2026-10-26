<script lang="ts">
import type { Snippet } from 'svelte'

interface Props {
  title: string
  /** Only for essential risk or data-flow notes the title cannot carry; omitted by default. */
  description?: string
  actions?: Snippet
  /** Resource or live status shown on the title row, after the title. */
  meta?: Snippet
}

let { title, description, actions, meta }: Props = $props()
</script>

<header class="route-page-header">
  <div class="min-w-0">
    <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
      <h1
        class="min-w-0 font-structural text-[1.5rem] leading-[1.15] font-semibold tracking-[-0.025em] text-balance [overflow-wrap:anywhere] sm:text-[1.75rem]">
        {title}
      </h1>
      {#if meta}{@render meta()}{/if}
    </div>
    {#if description}
      <p class="mt-2 max-w-3xl text-sm leading-[1.5] text-pretty text-muted-foreground">{description}</p>
    {/if}
  </div>
  {#if actions}
    <div class="flex shrink-0 flex-wrap items-center gap-2">{@render actions()}</div>
  {/if}
</header>
