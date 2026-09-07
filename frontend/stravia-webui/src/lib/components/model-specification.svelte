<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import CircleQuestionMarkIcon from '@lucide/svelte/icons/circle-question-mark'
import GaugeIcon from '@lucide/svelte/icons/gauge'

import { formatList, formatNumber } from '$lib/format'
import {
  formatSpecificationTokens,
  specificationFeatures,
  specificationModalities,
  specificationModality,
} from '$lib/model-specification'
import type { ModelSpecification } from '$lib/types'
import * as Tooltip from '$lib/components/ui/tooltip'
import { Badge } from '$lib/components/ui/badge'

type Density = 'compact' | 'detail'

interface Props {
  specification: ModelSpecification
  density?: Density
}

let { specification, density = 'compact' }: Props = $props()

const limit = $derived(specification.limit)
const modalities = $derived(specification.modalities)
const inputModalities = $derived(orderedModalities(modalities?.input ?? []))
const outputModalities = $derived(orderedModalities(modalities?.output ?? []))
const unknownFeatures = $derived(specificationFeatures.filter(({ key }) => specification[key] == null))
const supportedFeatures = $derived(specificationFeatures.filter(({ key }) => specification[key] === true))

function fullTokens(value: number | null | undefined): string {
  return value == null
    ? m.model_specification_not_registered()
    : m.model_specification_token_value({ value: formatNumber(value) })
}

function limitSummary(): string {
  return [
    m.model_specification_context_value({ value: fullTokens(limit?.context) }),
    m.model_specification_maximum_input_value({ value: fullTokens(limit?.input) }),
    m.model_specification_maximum_output_value({ value: fullTokens(limit?.output) }),
  ].join('; ')
}

function orderedModalities(values: string[]): string[] {
  return values.toSorted((left, right) => {
    const leftIndex = specificationModalities.findIndex(({ key }) => key === left.toLocaleLowerCase())
    const rightIndex = specificationModalities.findIndex(({ key }) => key === right.toLocaleLowerCase())
    return (
      (leftIndex < 0 ? specificationModalities.length : leftIndex) -
      (rightIndex < 0 ? specificationModalities.length : rightIndex)
    )
  })
}

function modalityDirectionLabel(modality: string, direction: 'input' | 'output'): string {
  const label = specificationModality(modality).label()
  return direction === 'input'
    ? m.model_specification_input_modality_value({ modality: label })
    : m.model_specification_output_modality_value({ modality: label })
}

function featureStatus(value: boolean | null | undefined): string {
  if (value == null) return m.model_specification_not_registered()
  return value ? m.model_specification_supported() : m.model_specification_not_supported()
}
</script>

{#snippet compactModalities(direction: 'input' | 'output', values: string[])}
  <div class="flex min-w-0 items-center gap-1.5">
    <span class="shrink-0 text-xs text-muted-foreground">
      {direction === 'input' ? m.model_specification_input() : m.model_specification_output()}
    </span>
    {#if values && values.length > 0}
      <span class="flex min-w-0 flex-wrap gap-1">
        {#each values as modality (`${direction}-${modality}`)}
          {@const entry = specificationModality(modality)}
          {@const ModalityIcon = entry.icon}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="inline-flex size-7 cursor-default items-center justify-center rounded-md bg-muted text-muted-foreground outline-none hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50"
              aria-label={modalityDirectionLabel(modality, direction)}>
              <ModalityIcon class="size-3.5" aria-hidden="true" />
            </Tooltip.Trigger>
            <Tooltip.Content role="tooltip" side="top" sideOffset={8}
              >{modalityDirectionLabel(modality, direction)}</Tooltip.Content>
          </Tooltip.Root>
        {/each}
      </span>
    {:else}
      <span class="text-xs text-muted-foreground">{m.model_specification_not_registered()}</span>
    {/if}
  </div>
{/snippet}

{#snippet detailModalities(direction: 'input' | 'output', values: string[])}
  <div class="flex flex-col gap-2">
    <dt class="text-xs font-medium text-muted-foreground">
      {direction === 'input' ? m.model_specification_input_modalities() : m.model_specification_output_modalities()}
    </dt>
    <dd class="flex flex-wrap gap-2">
      {#if values && values.length > 0}
        {#each values as modality (`${direction}-${modality}`)}
          {@const entry = specificationModality(modality)}
          {@const ModalityIcon = entry.icon}
          <Badge variant="outline" class="gap-2 px-2.5 py-1.5">
            <ModalityIcon class="size-4 text-muted-foreground" aria-hidden="true" />
            {entry.label()}
          </Badge>
        {/each}
      {:else}
        <span class="text-sm text-muted-foreground">{m.model_specification_not_registered()}</span>
      {/if}
    </dd>
  </div>
{/snippet}

{#if density === 'compact'}
  <div
    role="group"
    aria-label={m.model_specification_title()}
    class="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
    <Tooltip.Root>
      <Tooltip.Trigger
        type="button"
        class="inline-flex min-h-7 cursor-default items-center gap-1.5 rounded-md bg-muted px-2 font-technical text-xs tabular-nums text-muted-foreground outline-none hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50"
        aria-label={`${m.model_specification_token_limits()}: ${limitSummary()}`}>
        <GaugeIcon class="size-3.5" aria-hidden="true" />
        <span
          >{m.model_specification_context_short()}
          {limit?.context == null
            ? m.model_specification_not_registered()
            : formatSpecificationTokens(limit.context)}</span>
        <span aria-hidden="true">·</span>
        <span
          >{m.model_specification_maximum_output_short()}
          {limit?.output == null
            ? m.model_specification_not_registered()
            : formatSpecificationTokens(limit.output)}</span>
      </Tooltip.Trigger>
      <Tooltip.Content role="tooltip" side="top" sideOffset={8} class="max-w-80">
        <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-left">
          <dt>{m.model_specification_context()}</dt>
          <dd class="font-technical tabular-nums">{fullTokens(limit?.context)}</dd>
          <dt>{m.model_specification_maximum_input()}</dt>
          <dd class="font-technical tabular-nums">{fullTokens(limit?.input)}</dd>
          <dt>{m.model_specification_maximum_output()}</dt>
          <dd class="font-technical tabular-nums">{fullTokens(limit?.output)}</dd>
        </dl>
      </Tooltip.Content>
    </Tooltip.Root>

    <div class="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1.5">
      {@render compactModalities('input', inputModalities)}
      {@render compactModalities('output', outputModalities)}
    </div>

    {#if supportedFeatures.length > 0 || unknownFeatures.length > 0}
      <span class="flex flex-wrap items-center gap-1">
        <span class="mr-0.5 text-xs text-muted-foreground">{m.model_specification_features()}</span>
        {#each supportedFeatures as feature (feature.key)}
          {@const FeatureIcon = feature.icon}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="inline-flex size-7 cursor-default items-center justify-center rounded-md bg-muted text-muted-foreground outline-none hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50"
              aria-label={feature.label()}>
              <FeatureIcon class="size-3.5" aria-hidden="true" />
            </Tooltip.Trigger>
            <Tooltip.Content role="tooltip" side="top" sideOffset={8}>{feature.label()}</Tooltip.Content>
          </Tooltip.Root>
        {/each}
        {#if unknownFeatures.length > 0}
          {@const unknownLabel = m.model_specification_not_registered_features({
            features: formatList(unknownFeatures.map(({ label }) => label())),
          })}
          <Tooltip.Root>
            <Tooltip.Trigger
              type="button"
              class="inline-flex size-7 cursor-default items-center justify-center rounded-md bg-muted text-muted-foreground outline-none hover:text-foreground focus-visible:ring-[3px] focus-visible:ring-ring/50"
              aria-label={unknownLabel}>
              <CircleQuestionMarkIcon class="size-3.5" aria-hidden="true" />
            </Tooltip.Trigger>
            <Tooltip.Content role="tooltip" side="top" sideOffset={8} class="max-w-80">{unknownLabel}</Tooltip.Content>
          </Tooltip.Root>
        {/if}
      </span>
    {/if}
  </div>
{:else}
  <section class="flex flex-col gap-5" aria-label={m.model_specification_title()}>
    <div>
      <h3 class="text-sm font-semibold">{m.model_specification_title()}</h3>
      <p class="mt-1 text-xs text-muted-foreground">{m.model_specification_declaration_caveat()}</p>
      <p class="mt-1 text-xs text-muted-foreground">{m.model_specification_attachment_caveat()}</p>
    </div>

    <div class="flex flex-col gap-3">
      <h4 class="text-sm font-medium">{m.model_specification_token_limits()}</h4>
      <dl class="grid gap-3 sm:grid-cols-3">
        <div class="rounded-lg border p-3">
          <dt class="text-xs text-muted-foreground">{m.model_specification_context()}</dt>
          <dd class="mt-1 font-technical text-sm tabular-nums">{fullTokens(limit?.context)}</dd>
        </div>
        <div class="rounded-lg border p-3">
          <dt class="text-xs text-muted-foreground">{m.model_specification_maximum_input()}</dt>
          <dd class="mt-1 font-technical text-sm tabular-nums">{fullTokens(limit?.input)}</dd>
        </div>
        <div class="rounded-lg border p-3">
          <dt class="text-xs text-muted-foreground">{m.model_specification_maximum_output()}</dt>
          <dd class="mt-1 font-technical text-sm tabular-nums">{fullTokens(limit?.output)}</dd>
        </div>
      </dl>
    </div>

    <div class="flex flex-col gap-3 border-t pt-4">
      <h4 class="text-sm font-medium">{m.model_specification_modalities()}</h4>
      <dl class="grid gap-4 sm:grid-cols-2">
        {@render detailModalities('input', inputModalities)}
        {@render detailModalities('output', outputModalities)}
      </dl>
    </div>

    <div class="flex flex-col gap-3 border-t pt-4">
      <h4 class="text-sm font-medium">{m.model_specification_features()}</h4>
      <dl class="grid gap-2 sm:grid-cols-2">
        {#each specificationFeatures as feature (feature.key)}
          {@const FeatureIcon = feature.icon}
          <div class="flex items-center justify-between gap-3 rounded-lg border px-3 py-2">
            <dt class="flex items-center gap-2 text-sm">
              <FeatureIcon class="size-4 text-muted-foreground" aria-hidden="true" />
              {feature.label()}
            </dt>
            <dd class="text-sm text-muted-foreground">{featureStatus(specification[feature.key])}</dd>
          </div>
        {/each}
      </dl>
    </div>
  </section>
{/if}
