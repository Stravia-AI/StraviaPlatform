<script lang="ts">
import PlugIcon from '@lucide/svelte/icons/plug'
import { icons } from '../../assets/icons'
import { catalogLogoUrl } from '$lib/admin-client'
import { cn } from '$lib/utils'

interface Props {
  class?: string
  /** Stable provider identity used for the built-in SVG and name fallback. */
  icon?: string | null
  name: string
  /** Standalone SVG source passed directly by caller (e.g. plugin-registered icon_svg) */
  svg?: string | null
  /**
   * Host icon request key: a descriptor/catalog identity for unsaved options
   * or the provider connection id for saved connections (the host resolves
   * the catalog identity, website, and connection origin itself).
   */
  logo?: string | null
}

let { class: className, icon, name, svg, logo }: Props = $props()
let failedSource = $state<string>()
const isCustom = $derived(icon?.toLowerCase() === 'custom')
const resolvedSvg = $derived(svg ?? (icon && !isCustom ? icons[icon.toLowerCase()] : undefined))
const svgSource = $derived(resolvedSvg ? `data:image/svg+xml,${encodeURIComponent(resolvedSvg)}` : undefined)
const remoteSource = $derived(logo ? catalogLogoUrl(logo) : undefined)
const usingFallback = $derived(Boolean(failedSource) || (!svgSource && !remoteSource))
</script>

{#snippet fallback()}
  {#if svgSource}
    <img src={svgSource} alt="" />
  {:else if isCustom}
    <PlugIcon class="size-4" />
  {:else}
    <span class="font-structural font-semibold">{(name.trim().slice(0, 1) || '?').toUpperCase()}</span>
  {/if}
{/snippet}

{#snippet remoteLogo(source: string)}
  <img src={source} alt="" loading="lazy" decoding="async" onerror={() => (failedSource = source)} />
{/snippet}

<span
  class={cn('route-provider-mark text-[0.7rem]', className)}
  data-fallback={usingFallback ? 'true' : 'false'}
  aria-hidden="true">
  {#if svgSource}
    {@render fallback()}
  {:else if remoteSource}
    {#await remoteSource}
      {@render fallback()}
    {:then source}
      {#if failedSource === source}
        {@render fallback()}
      {:else}
        {@render remoteLogo(source)}
      {/if}
    {:catch}
      {@render fallback()}
    {/await}
  {:else}
    {@render fallback()}
  {/if}
</span>
