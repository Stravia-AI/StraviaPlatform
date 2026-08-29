<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import FileQuestionIcon from '@lucide/svelte/icons/file-question'
import FileTextIcon from '@lucide/svelte/icons/file-text'
import ImageIcon from '@lucide/svelte/icons/image'
import TypeIcon from '@lucide/svelte/icons/type'
import VideoIcon from '@lucide/svelte/icons/video'
import Volume2Icon from '@lucide/svelte/icons/volume-2'

import { admin } from '$lib/admin-client'
import { localizeBackendErrorMessage } from '$lib/backend-error'
import { formatNumber } from '$lib/format'
import type { ProviderModelDetail, ProviderModelMetadata, ProviderModelPrices } from '$lib/types'
import { Badge } from '$lib/components/ui/badge'
import { Button } from '$lib/components/ui/button'
import * as Dialog from '$lib/components/ui/dialog'
import { Spinner } from '$lib/components/ui/spinner'
import * as Tooltip from '$lib/components/ui/tooltip'

interface Props {
  providerId: string
  modelId: string
  triggerLabel: string
}

type FeatureKey = 'attachment' | 'reasoning' | 'tool_call' | 'structured_output' | 'temperature'
type PriceKey = keyof ProviderModelPrices

const featureFields: Array<{ key: FeatureKey; label: () => string }> = [
  { key: 'attachment', label: m.provider_model_field_attachments },
  { key: 'reasoning', label: m.provider_model_field_reasoning },
  { key: 'tool_call', label: m.provider_model_field_tool_calls },
  { key: 'structured_output', label: m.provider_model_field_structured_output },
  { key: 'temperature', label: m.provider_model_field_temperature },
]
const priceFields: Array<{ key: PriceKey; label: () => string }> = [
  { key: 'input', label: m.provider_model_field_input },
  { key: 'output', label: m.provider_model_field_output },
  { key: 'reasoning', label: m.provider_model_field_reasoning },
  { key: 'cache_read', label: m.provider_model_field_cache_read },
  { key: 'cache_write', label: m.provider_model_field_cache_write },
  { key: 'input_audio', label: m.provider_model_field_audio_input },
  { key: 'output_audio', label: m.provider_model_field_audio_output },
]
const modalityIcons = {
  text: TypeIcon,
  image: ImageIcon,
  video: VideoIcon,
  audio: Volume2Icon,
  pdf: FileTextIcon,
  document: FileTextIcon,
  file: FileTextIcon,
}

let { providerId, modelId, triggerLabel }: Props = $props()
let open = $state(false)
let detail = $state<ProviderModelDetail>()
let loading = $state(false)
let error = $state('')

const metadata = $derived<ProviderModelMetadata>(detail?.metadata ?? {})
const supportedFeatures = $derived(featureFields.filter(({ key }) => metadata[key] === true))
const prices = $derived.by(() => {
  const cost = metadata.cost
  if (!cost) return []
  return priceFields.flatMap(({ key, label }) => {
    const value = cost[key]
    return value == null ? [] : [{ key, label: label(), value }]
  })
})
const hasModalities = $derived(Boolean(metadata.modalities?.input.length || metadata.modalities?.output.length))
const hasLimits = $derived(
  Boolean(
    metadata.limit && (metadata.limit.context != null || metadata.limit.input != null || metadata.limit.output != null),
  ),
)

function modalityIcon(modality: string) {
  return modalityIcons[modality.toLocaleLowerCase() as keyof typeof modalityIcons] ?? FileQuestionIcon
}

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

{#snippet modalityGroup(label: string, modalities: string[])}
  {#if modalities.length > 0}
    <div class="flex flex-col gap-2">
      <p class="text-xs font-medium text-muted-foreground">{label}</p>
      <div class="flex flex-wrap gap-2">
        {#each modalities as modality (modality)}
          {@const ModalityIcon = modalityIcon(modality)}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="inline-flex size-10 cursor-default items-center justify-center rounded-lg border bg-background text-muted-foreground shadow-xs outline-none transition-colors hover:bg-accent hover:text-accent-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50 [&_svg]:size-5"
              aria-label={modality.toLocaleUpperCase()}>
              <ModalityIcon />
            </Tooltip.Trigger>
            <Tooltip.Content side="top" sideOffset={8}>{modality.toLocaleUpperCase()}</Tooltip.Content>
          </Tooltip.Root>
        {/each}
      </div>
    </div>
  {/if}
{/snippet}

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
          {#if hasModalities && metadata.modalities}
            <section class="flex flex-wrap gap-x-8 gap-y-4">
              {@render modalityGroup(m.provider_model_editor_accepted_input_types(), metadata.modalities.input)}
              {@render modalityGroup(m.provider_model_editor_generated_output_types(), metadata.modalities.output)}
            </section>
          {/if}

          {#if supportedFeatures.length > 0}
            <section class={['flex flex-col gap-3', hasModalities && 'border-t pt-4']}>
              <h3 class="text-sm font-semibold">{m.provider_model_editor_supported_features()}</h3>
              <div class="flex flex-wrap gap-2">
                {#each supportedFeatures as feature (feature.key)}
                  <Badge variant="outline">{feature.label()}</Badge>
                {/each}
              </div>
            </section>
          {/if}

          {#if hasLimits && metadata.limit}
            <section class="flex flex-col gap-3 border-t pt-4">
              <h3 class="text-sm font-semibold">{m.provider_model_editor_token_limits()}</h3>
              <div class="rounded-lg border p-4">
                <dl class="grid grid-cols-3 gap-3 text-sm">
                  <div>
                    <dt class="text-xs text-muted-foreground">{m.common_context()}</dt>
                    <dd class="mt-1 font-technical">{formatNumber(metadata.limit.context)}</dd>
                  </div>
                  <div>
                    <dt class="text-xs text-muted-foreground">{m.provider_model_field_input()}</dt>
                    <dd class="mt-1 font-technical">{formatNumber(metadata.limit.input)}</dd>
                  </div>
                  <div>
                    <dt class="text-xs text-muted-foreground">{m.provider_model_field_output()}</dt>
                    <dd class="mt-1 font-technical">{formatNumber(metadata.limit.output)}</dd>
                  </div>
                </dl>
              </div>
            </section>
          {/if}

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
