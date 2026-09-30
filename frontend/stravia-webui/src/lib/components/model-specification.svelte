<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { formatNumber } from '$lib/format'
import { formatSpecificationTokens, specificationModality } from '$lib/model-specification'
import type { ModelSpecification } from '$lib/types'

interface Props {
  specification: ModelSpecification
  density?: 'compact' | 'target' | 'detail'
}
let { specification, density = 'compact' }: Props = $props()
const context = $derived(specification.limit?.context)
const efforts = $derived(specification.reasoning_efforts ?? [])
function modalityText(values: string[] | undefined): string {
  return values?.length
    ? values.map((value) => specificationModality(value).label()).join(', ')
    : m.model_specification_not_registered()
}
</script>

{#if density === 'detail'}
  <section aria-label={m.model_specification_title()} class="flex flex-col gap-4">
    <dl class="grid gap-4 sm:grid-cols-2">
      <div>
        <dt class="text-xs text-muted-foreground">{m.model_specification_context()}</dt>
        <dd class="mt-1 font-technical text-sm">
          {context == null
            ? m.model_specification_not_registered()
            : m.model_specification_token_value({ value: formatNumber(context) })}
        </dd>
      </div>
      <div>
        <dt class="text-xs text-muted-foreground">{m.model_specification_reasoning_efforts()}</dt>
        <dd class="mt-1 break-words font-technical text-sm">
          {efforts.length ? efforts.join(', ') : m.model_specification_not_registered()}
        </dd>
      </div>
      <div>
        <dt class="text-xs text-muted-foreground">{m.model_specification_input_modalities()}</dt>
        <dd class="mt-1 text-sm">{modalityText(specification.modalities?.input)}</dd>
      </div>
      <div>
        <dt class="text-xs text-muted-foreground">{m.model_specification_output_modalities()}</dt>
        <dd class="mt-1 text-sm">{modalityText(specification.modalities?.output)}</dd>
      </div>
    </dl>
  </section>
{:else}
  <div
    role="group"
    aria-label={m.model_specification_title()}
    class="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-xs">
    <span class="font-technical"
      >{m.model_specification_context_short()}
      {context == null ? m.model_specification_not_registered() : formatSpecificationTokens(context)}</span>
    <span
      ><span class="text-muted-foreground">{m.model_specification_input()}</span>
      {modalityText(specification.modalities?.input)}</span>
    <span
      ><span class="text-muted-foreground">{m.model_specification_output()}</span>
      {modalityText(specification.modalities?.output)}</span>
    {#if density !== 'target'}
      <span class="break-words"
        ><span class="text-muted-foreground">{m.model_specification_reasoning_efforts()}</span>
        <span class="font-technical"
          >{efforts.length ? efforts.join(', ') : m.model_specification_not_registered()}</span
        ></span>
    {/if}
  </div>
{/if}
