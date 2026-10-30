<script lang="ts">
import type { Snippet } from 'svelte'
import * as m from '#lib/paraglide/messages.js'
import type { HitMark } from '#lib/credential-hit-highlights.js'
import { badgeVariants } from '#lib/components/ui/badge/variants.js'
import * as Tooltip from '#lib/components/ui/tooltip/index.js'
import { cn } from '#lib/utils.js'

let {
  marks,
  ruleNames,
  children,
}: {
  /** 同一块上的命中（如工具调用参数与结果各一处）；全为 undefined 时只透传内容，不引入布局。 */
  marks: readonly (HitMark | undefined)[]
  ruleNames: ReadonlyMap<string, string>
  children: Snippet
} = $props()

// 参数与结果可能命中同一规则：相同徽标只留一个，同时保证 each 键唯一。
const hits = $derived([
  ...new Map(
    marks
      .filter((mark) => mark !== undefined)
      .map((mark) => {
        const hit = {
          kind: mark.kind,
          label:
            mark.kind === 'created' ? m.credential_protection_hits_created() : m.credential_protection_hits_reused(),
          rules: mark.ruleIds.map((id) => ruleNames.get(id) ?? id).join(', '),
        }
        return [`${hit.kind}:${hit.rules}`, hit] as const
      }),
  ).values(),
])
// 新增优先于复用：块的高亮跟随最强的一处命中。
const kind = $derived(hits.some((hit) => hit.kind === 'created') ? 'created' : hits[0]?.kind)
</script>

<div class="hit" data-credential-hit={kind}>
  {#each hits as hit (`${hit.kind}:${hit.rules}`)}
    <Tooltip.Root delayDuration={0}>
      <Tooltip.Trigger
        type="button"
        class={cn(badgeVariants({ variant: hit.kind === 'created' ? 'default' : 'secondary' }), 'hit-badge')}
        aria-label="{hit.label}: {hit.rules}">
        {hit.label}
      </Tooltip.Trigger>
      <Tooltip.Content class="max-w-80 text-pretty">{hit.rules}</Tooltip.Content>
    </Tooltip.Root>
  {/each}
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
