<script lang="ts">
import type { Snippet } from 'svelte'

import * as Field from '#lib/components/ui/field/index.js'

interface Props {
  label: string
  /** 控件（Select 触发器）的 id，label 的 `for`。 */
  labelFor: string
  /** 可选解释，只在 label 右侧问号提示中出现。 */
  hint?: string
  /** Select 触发器；在宽行中固定为 16rem 并贴右缘。 */
  children: Snippet
  /** 行下方的禁用原因或次要动作，宽行中靠右与控件对齐。 */
  below?: Snippet
}

let { label, labelFor, hint, children, below }: Props = $props()
</script>

<!--
  接入控制行：行宽达到 36rem 时左侧 label、右侧 16rem 有界控件并贴右缘，同列各行共用同一控件边缘；
  更窄时 label 在上、控件占满行（不超过 28rem）。
-->
<div data-slot="connect-control-row" class="@container/control-row min-w-0">
  <div
    class="flex min-w-0 flex-col gap-2 @min-[36rem]/control-row:flex-row @min-[36rem]/control-row:items-center @min-[36rem]/control-row:justify-between @min-[36rem]/control-row:gap-6">
    <Field.FieldLabel for={labelFor} {hint}>{label}</Field.FieldLabel>
    <div
      data-slot="connect-control"
      class="w-full max-w-[28rem] min-w-0 @min-[36rem]/control-row:w-64 @min-[36rem]/control-row:shrink-0">
      {@render children()}
    </div>
  </div>
  {#if below}
    <div class="mt-2 flex min-w-0 flex-col gap-2 @min-[36rem]/control-row:items-end">{@render below()}</div>
  {/if}
</div>
