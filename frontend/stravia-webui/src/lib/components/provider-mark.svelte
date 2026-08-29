<script lang="ts">
import { icons } from '../../assets/icons'
import { catalogLogoUrl } from '$lib/admin-client'

interface Props {
  icon?: string | null
  name: string
  catalog?: boolean
  endpoint?: string | null
}

let { icon, name, catalog = false, endpoint }: Props = $props()
let failedSource = $state<string>()
const isCustom = $derived(icon?.toLowerCase() === 'custom')
const svg = $derived(icon && !isCustom ? icons[icon.toLowerCase()] : undefined)
const svgSource = $derived(svg ? `data:image/svg+xml,${encodeURIComponent(svg)}` : undefined)
const catalogSource = $derived(catalog && icon && !isCustom ? catalogLogoUrl(icon) : undefined)
const endpointSource = $derived(isCustom ? endpointFaviconUrl(endpoint) : undefined)
const usingFallback = $derived(
  Boolean(failedSource) || (!svgSource && !catalogSource && !endpointSource),
)

function endpointFaviconUrl(endpoint: string | null | undefined): string | undefined {
  if (!endpoint) return undefined

  try {
    const url = new URL(endpoint)
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return undefined
    return new URL('/favicon.ico', url.origin).href
  } catch {
    return undefined
  }
}
</script>

{#snippet fallback()}
  {#if svgSource}
    <img src={svgSource} alt="" />
  {:else}
    <span class="font-structural text-[0.7rem] font-semibold">{(name.trim().slice(0, 1) || '?').toUpperCase()}</span>
  {/if}
{/snippet}

{#snippet remoteLogo(source: string)}
  <img src={source} alt="" loading="lazy" decoding="async" onerror={() => (failedSource = source)} />
{/snippet}

<span class="route-provider-mark" data-fallback={usingFallback ? 'true' : 'false'} aria-hidden="true">
  {#if catalogSource}
    {#await catalogSource}
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
  {:else if endpointSource && failedSource !== endpointSource}
    {@render remoteLogo(endpointSource)}
  {:else}
    {@render fallback()}
  {/if}
</span>
