<script lang="ts">
import type { Snippet } from 'svelte'

import { cn } from '#lib/utils.js'

interface Props {
  title: string
  children: Snippet
  class?: string
}

let { title, children, class: className }: Props = $props()
const titleId = $props.id()
</script>

<!--
  长表单的小节：14px/600 标题、无描述、标题到内容 12px；小节之间的 24px 由父级 gap 提供，不画分隔线，避免与行线叠成双线。
  小节内的字段标签降为 400：标题已经是 600，标签再加粗会和它叠成两行同样重的标题；设置行标题不受影响。
-->
<section
  class={cn('flex min-w-0 flex-col gap-3 [&_[data-slot=field-label]_[data-slot=label]]:font-normal', className)}
  aria-labelledby={titleId}>
  <h3 id={titleId} class="min-w-0 text-[0.875rem] font-semibold">{title}</h3>
  {@render children()}
</section>
