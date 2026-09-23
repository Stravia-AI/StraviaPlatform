<script lang="ts">
import PlugIcon from '@lucide/svelte/icons/plug'
import { icons } from '../../assets/icons'
import { catalogLogoUrl } from '$lib/admin-client'

interface Props {
  /** Stable provider identity used for the built-in SVG and name fallback. */
  icon?: string | null
  name: string
  /**
   * Host icon request key: a descriptor/catalog identity for unsaved options
   * or the provider connection id for saved connections (the host resolves
   * the catalog identity, website, and connection origin itself).
   */
  logo?: string | null
}

let { icon, name, logo }: Props = $props()
let failedSource = $state<string>()
const isCustom = $derived(icon?.toLowerCase() === 'custom')
const svg = $derived(icon && !isCustom ? icons[icon.toLowerCase()] : undefined)
const svgSource = $derived(svg ? `data:image/svg+xml,${encodeURIComponent(svg)}` : undefined)
const remoteSource = $derived(logo ? catalogLogoUrl(logo) : undefined)
const usingFallback = $derived(Boolean(failedSource) || (!svgSource && !remoteSource))
</script>

{#snippet fallback()}
  {#if svgSource}
    <img src={svgSource} alt="" />
  {:else if isCustom}
    <PlugIcon class="size-4" />
  {:else}
    <span class="font-structural text-[0.7rem] font-semibold">{(name.trim().slice(0, 1) || '?').toUpperCase()}</span>
  {/if}
{/snippet}

{#snippet remoteLogo(source: string)}
  <img src={source} alt="" loading="lazy" decoding="async" onerror={() => (failedSource = source)} />
{/snippet}

<span class="route-provider-mark" data-fallback={usingFallback ? 'true' : 'false'} aria-hidden="true">
  {#if remoteSource}
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
