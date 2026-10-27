<script lang="ts">
import type { Snippet } from 'svelte'
import * as m from '#lib/paraglide/messages.js'
import type { HitMark } from '#lib/credential-hit-highlights.js'
import { badgeVariants } from '#lib/components/ui/badge/variants.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'
import { cn } from '#lib/utils.js'

let {
  mark,
  ruleNames,
  children,
}: {
  /** 没有命中时只透传内容，不引入布局。 */
  mark?: HitMark
  ruleNames: ReadonlyMap<string, string>
  children: Snippet
} = $props()

const label = $derived(
  mark?.kind === 'created' ? m.credential_protection_hits_created() : m.credential_protection_hits_reused(),
)
const rules = $derived(mark?.ruleIds.map((id) => ruleNames.get(id) ?? id).join(', ') ?? '')
</script>

<div class="hit" data-credential-hit={mark?.kind}>
  {#if mark}
    <Tooltip.Root delayDuration={0}>
      <Tooltip.Trigger
        type="button"
        class={cn(badgeVariants({ variant: mark.kind === 'created' ? 'default' : 'secondary' }), 'hit-badge')}
        aria-label="{label}: {rules}">
        {label}
      </Tooltip.Trigger>
      <Tooltip.Content class="max-w-80 text-pretty">{rules}</Tooltip.Content>
    </Tooltip.Root>
  {/if}
  {@render children()}
</div>

<style>
.hit:not([data-credential-hit]) {
  display: contents;
}
.hit[data-credential-hit] {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 0.375rem;
  padding: 0.5rem 0.625rem;
  border-radius: var(--radius-lg);
}
/* 与 Run 跳转高亮同一套视觉语言：新增用强调色，复用弱化。 */
.hit[data-credential-hit='created'] {
  background: var(--accent);
  box-shadow: inset 2px 0 0 var(--primary);
}
.hit[data-credential-hit='reused'] {
  background: var(--muted);
  box-shadow: inset 2px 0 0 var(--muted-foreground);
}
.hit :global(.hit-badge) {
  align-self: flex-start;
  cursor: default;
}
</style>
