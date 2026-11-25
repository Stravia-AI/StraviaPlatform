<script lang="ts" module>
const NESTED_SETTINGS_SECTION = Symbol('settings-section')
</script>

<script lang="ts">
import { hasContext, setContext, type Snippet } from 'svelte'

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
const nested = hasContext(NESTED_SETTINGS_SECTION)
setContext(NESTED_SETTINGS_SECTION, true)
</script>

<!-- 设置区段：顶层区段以次级标题与分组面板包裹内容；嵌套区段沿用外层面板，不再套第二层卡片边框。 -->
<section {id} data-slot="settings-section" aria-labelledby={titleId} class="min-w-0 scroll-mt-4">
  {#if nested}
    <div class="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-4 pt-3 pb-1">
      <div class="flex min-w-0 items-center gap-2">
        <h2 id={titleId} class="text-xs leading-snug font-medium tracking-normal text-muted-foreground">{title}</h2>
        {#if badge}<Badge variant="secondary">{badge}</Badge>{/if}
      </div>
      {#if action}<div class="flex shrink-0 items-center gap-2">{@render action()}</div>{/if}
    </div>
    {@render children()}
  {:else}
    <div class="mb-2 flex min-h-7 flex-wrap items-center justify-between gap-x-4 gap-y-2">
      <div class="flex min-w-0 items-center gap-2">
        <h2 id={titleId} class="text-[0.8125rem] leading-snug font-medium tracking-normal text-muted-foreground">
          {title}
        </h2>
        {#if badge}<Badge variant="secondary">{badge}</Badge>{/if}
      </div>
      {#if action}<div class="flex shrink-0 items-center gap-2">{@render action()}</div>{/if}
    </div>
    <div
      data-slot="settings-section-panel"
      class="min-w-0 overflow-hidden rounded-xl border border-border/75 bg-card/70 dark:bg-card/55">
      {@render children()}
    </div>
  {/if}
</section>
