<script lang="ts">
import type { Snippet } from 'svelte'

import { cn } from '#lib/utils.js'

interface Props {
  title: string
  /** 作用于整个小节的次要操作（例如移除整组配置），与标题同行靠右。 */
  actions?: Snippet
  children: Snippet
  class?: string
}

let { title, actions, children, class: className }: Props = $props()
const titleId = $props.id()
</script>

<!-- 长表单的小节：14px/600 标题、无描述、标题到内容 12px；小节之间的 24px 由父级 gap 提供，不画分隔线，避免与行线叠成双线。 -->
<section class={cn('flex min-w-0 flex-col gap-3', className)} aria-labelledby={titleId}>
  <div class="flex items-center justify-between gap-4">
    <h3 id={titleId} class="min-w-0 text-sm font-semibold">{title}</h3>
    {#if actions}<div class="flex shrink-0 flex-wrap gap-2">{@render actions()}</div>{/if}
  </div>
  {@render children()}
</section>
