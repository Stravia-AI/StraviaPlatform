<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { createQuery } from '@tanstack/svelte-query'
import { admin } from '#lib/admin-client.js'
import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { formatNumber } from '#lib/format.js'
import { priceFieldCurrencySymbol } from '#lib/components/price-form.js'
import type { PriceFieldName, ProviderModelDetail, ProviderModelMetadata, ProviderModelPrices } from '#lib/types.js'
import ModelSpecification from '#lib/components/model-specification.svelte'
import { Button } from '#lib/components/ui/button/index.js'
import * as Dialog from '#lib/components/ui/dialog/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

interface Props {
  providerId: string
  modelId: string
  triggerLabel: string
}

type PriceKey = keyof ProviderModelPrices

const priceFields: Array<{ key: PriceKey; label: () => string }> = [
  { key: 'input', label: m.provider_model_field_input },
  { key: 'output', label: m.provider_model_field_output },
  { key: 'cache_read', label: m.provider_model_field_cache_read },
  { key: 'cache_write', label: m.provider_model_field_cache_write },
]
let { providerId, modelId, triggerLabel }: Props = $props()
let open = $state(false)
let detail = $state<ProviderModelDetail>()
let loading = $state(false)
let error = $state('')
const currencyQuery = createQuery(() => ({
  queryKey: ['currency'],
  queryFn: () => admin.currency.get(),
  enabled: open,
}))

const metadata = $derived<ProviderModelMetadata>(detail?.metadata ?? {})
const priceGroups = $derived.by(() => {
  const cost = metadata.cost
  if (!cost) return []
  const customFields = new Set<PriceFieldName>(cost.custom_currency_fields ?? [])
  const customSymbol = currencyQuery.data?.symbol
  const currencySymbol = (field: PriceFieldName): string => priceFieldCurrencySymbol(field, customFields, customSymbol)
  const schedules: Array<{ label: string; prices: ProviderModelPrices; currencyField?: PriceFieldName }> = [
    { label: m.model_specification_base_prices(), prices: cost },
    ...(cost.context_over_200k
      ? [
          {
            label: m.model_specification_extended_context_prices(),
            prices: cost.context_over_200k,
            currencyField: 'tiers' as const,
          },
        ]
      : []),
    ...(cost.tiers ?? []).map((tier) => ({
      label: `${tier.tier.type} · ${formatNumber(tier.tier.size)}`,
      prices: tier,
      currencyField: 'tiers' as const,
    })),
  ]
  return schedules
    .map((schedule) => ({
      label: schedule.label,
      prices: priceFields.flatMap(({ key, label }) => {
        const value = schedule.prices[key]
        return value == null
          ? []
          : [{ key, label: label(), value: String(value), symbol: currencySymbol(schedule.currencyField ?? key) }]
      }),
    }))
    .filter((schedule) => schedule.prices.length > 0)
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
        {detail?.metadata.description || modelId}
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
          <dl class="grid gap-4 border-b pb-4 sm:grid-cols-2">
            <div>
              <dt class="text-xs text-muted-foreground">{m.common_model()}</dt>
              <dd class="mt-1 break-words text-sm">{metadata.name || detail.id}</dd>
            </div>
            <div>
              <dt class="text-xs text-muted-foreground">{m.model_specification_model_id()}</dt>
              <dd class="mt-1 break-all font-technical text-sm">{detail.id}</dd>
            </div>
            <div>
              <dt class="text-xs text-muted-foreground">{m.model_specification_family()}</dt>
              <dd class="mt-1 text-sm">{metadata.family || m.model_specification_not_registered()}</dd>
            </div>
            <div>
              <dt class="text-xs text-muted-foreground">{m.provider_model_catalog_model_availability()}</dt>
              <dd class="mt-1 text-sm">
                {detail.available ? m.model_specification_available() : m.common_unavailable()}
              </dd>
            </div>
          </dl>
          <ModelSpecification specification={metadata} density="detail" />

          {#if priceGroups.length > 0}
            <section class="flex flex-col gap-3 border-t pt-4">
              <div>
                <h3 class="text-sm font-semibold">{m.provider_model_editor_pricing()}</h3>
                <p class="mt-1 text-xs text-muted-foreground">
                  {m.provider_model_editor_pricing_unit_help()}
                </p>
              </div>
              {#each priceGroups as group (group.label)}
                <h4 class="text-xs font-medium">{group.label}</h4>
                <dl class="grid gap-3 sm:grid-cols-2">
                  {#each group.prices as price (price.key)}
                    <div class="flex items-center justify-between gap-3 border-b py-2">
                      <dt class="text-sm text-muted-foreground">{price.label}</dt>
                      <dd class="font-technical text-sm">{price.symbol}{price.value}</dd>
                    </div>
                  {/each}
                </dl>
              {/each}
            </section>
          {/if}
        </div>
      </div>
    {/if}
  </Dialog.Content>
</Dialog.Root>
