<script lang="ts">
import type { Snippet } from 'svelte'

import * as Field from '#lib/components/ui/field/index.js'

interface Props {
  label: string
  /** 控件（Select 触发器）的 id，label 的 `for`。 */
  labelFor: string
  /** 可选解释，只在 label 右侧问号提示中出现。 */
  hint?: string
  /** Select 触发器；占满所在列。 */
  children: Snippet
  /** 控件下方的禁用原因或次要动作。 */
  below?: Snippet
}

let { label, labelFor, hint, children, below }: Props = $props()
</script>

<!-- 接入控制行：label 在上、控件占满所在列，同列各行共用同一条左右边缘；列宽由页面决定。 -->
<div data-slot="connect-control-row" class="flex min-w-0 flex-col gap-2">
  <Field.FieldLabel for={labelFor} {hint}>{label}</Field.FieldLabel>
  <div data-slot="connect-control" class="w-full min-w-0">
    {@render children()}
  </div>
  {#if below}
    <div class="flex min-w-0 flex-col gap-2">{@render below()}</div>
  {/if}
</div>
