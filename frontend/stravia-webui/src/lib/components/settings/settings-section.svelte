<script lang="ts">
import type { Snippet } from 'svelte'

import { Badge } from '#lib/components/ui/badge/index.js'

interface Props {
  id: string
  /** 需要稳定标题 id 的区段（如被测试或外部引用）显式指定；默认自动生成。 */
  titleId?: string
  title: string
  /** 影响判断的标记（如 Beta），跟随标题。 */
  badge?: string
  /** 区段级动作（如多行摘要的「编辑」），与标题同行靠右。 */
  action?: Snippet
  children: Snippet
}

let { id, titleId: explicitTitleId, title, badge, action, children }: Props = $props()

const generatedTitleId = $props.id()
const titleId = $derived(explicitTitleId ?? generatedTitleId)
</script>

<!-- 设置区段：h2 + 设置行；默认没有描述、顶边线与卡片，节奏由行线建立。 -->
<section {id} aria-labelledby={titleId} class="min-w-0 scroll-mt-4">
  <div class="mb-1 flex min-h-8 flex-wrap items-center justify-between gap-x-4 gap-y-2">
    <div class="flex min-w-0 items-center gap-2">
      <h2 id={titleId} class="route-section-title">{title}</h2>
      {#if badge}<Badge variant="secondary">{badge}</Badge>{/if}
    </div>
    {#if action}<div class="flex shrink-0 items-center gap-2">{@render action()}</div>{/if}
  </div>
  {@render children()}
</section>
