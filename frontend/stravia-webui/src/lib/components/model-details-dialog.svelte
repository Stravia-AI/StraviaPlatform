<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatNumber } from '$lib/format'
import type { ProviderModelDetail, ProviderModelMetadata, ProviderModelPrices } from '$lib/types'
import ModelSpecification from '$lib/components/model-specification.svelte'
import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import { Spinner } from '$lib/components/ui/spinner'

interface Props {
  providerId: string
  modelId: string
  triggerLabel: string
}

type PriceKey = keyof ProviderModelPrices

const priceFields: Array<{ key: PriceKey; label: () => string }> = [
  { key: 'input', label: m.provider_model_field_input },
  { key: 'output', label: m.provider_model_field_output },
  { key: 'reasoning', label: m.provider_model_field_reasoning },
  { key: 'cache_read', label: m.provider_model_field_cache_read },
  { key: 'cache_write', label: m.provider_model_field_cache_write },
  { key: 'input_audio', label: m.provider_model_field_audio_input },
  { key: 'output_audio', label: m.provider_model_field_audio_output },
]
let { providerId, modelId, triggerLabel }: Props = $props()
let open = $state(false)
let detail = $state<ProviderModelDetail>()
let loading = $state(false)
let error = $state('')

const metadata = $derived<ProviderModelMetadata>(detail?.metadata ?? {})
const prices = $derived.by(() => {
  const cost = metadata.cost
  if (!cost) return []
  return priceFields.flatMap(({ key, label }) => {
    const value = cost[key]
    return value == null ? [] : [{ key, label: label(), value }]
  })
})
async function loadDetails(): Promise<void> {
  loading = true
  error = ''
  try {
    detail = await admin.providers.model(providerId, modelId)
  } catch (cause) {
    detail = undefined
    error = m.model_editor_model_details_load_failed({ error: localizeBackendErrorMessage(cause) })
  } finally {
    loading = false
  }
}
</script>

<Dialog.Root
  bind:open
  onOpenChange={(nextOpen) => {
    if (nextOpen) void loadDetails()
  }}>
  <Dialog.Trigger>
    {#snippet child({ props })}
      <Button {...props} type="button" variant="ghost" size="sm">{triggerLabel}</Button>
    {/snippet}
  </Dialog.Trigger>
  <Dialog.Content
    class="flex max-h-[calc(100vh-2rem)] flex-col gap-0 overflow-hidden p-0 sm:max-w-2xl [&_[data-slot=dialog-close]]:top-4 [&_[data-slot=dialog-close]]:right-4">
    <Dialog.Header class="shrink-0 border-b px-6 py-5 pr-16">
      <Dialog.Title>{detail?.metadata.name || m.provider_model_editor_model_information()}</Dialog.Title>
      <Dialog.Description class="text-pretty">
        {detail?.metadata.description || m.provider_model_editor_model_information()}
      </Dialog.Description>
    </Dialog.Header>

    {#if loading}
      <div class="grid min-h-56 flex-1 place-items-center px-6 py-5"><Spinner /></div>
    {:else if error}
      <div class="flex min-h-40 flex-1 flex-col items-center justify-center gap-4 px-6 py-5 text-center">
        <p class="text-sm text-destructive" role="alert">{error}</p>
        <Button type="button" variant="outline" onclick={() => void loadDetails()}>{m.common_try_again()}</Button>
      </div>
    {:else if detail}
      <div class="min-h-0 flex-1 overflow-y-auto overscroll-contain px-6 py-5">
        <div class="flex flex-col gap-5">
          <ModelSpecification specification={metadata} density="detail" />

          {#if prices.length > 0}
            <section class="flex flex-col gap-3 border-t pt-4">
              <div>
                <h3 class="text-sm font-semibold">{m.provider_model_editor_pricing()}</h3>
                <p class="mt-1 text-xs text-muted-foreground">
                  {m.provider_model_editor_pricing_unit_help()}
                </p>
              </div>
              <dl class="grid gap-3 sm:grid-cols-2">
                {#each prices as price (price.key)}
                  <div class="flex items-center justify-between gap-3 rounded-lg border px-4 py-3">
                    <dt class="text-sm text-muted-foreground">{price.label}</dt>
                    <dd class="font-technical text-sm">${formatNumber(price.value)}</dd>
                  </div>
                {/each}
              </dl>
            </section>
          {/if}
        </div>
      </div>
    {/if}
  </Dialog.Content>
</Dialog.Root>
