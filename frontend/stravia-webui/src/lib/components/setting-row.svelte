<script lang="ts" module>
import type { Snippet } from 'svelte'

import type { FieldControlSize } from '#lib/components/ui/field/field-sizes.js'

/** 传给 `children` 的标识：控件用 `aria-labelledby` / `aria-describedby` 关联标题与说明。 */
export interface SettingRowIds {
  titleId: string
  descriptionId: string | undefined
}

export interface SettingRowProps {
  /** 设置名（14px/500）。始终可见。 */
  title: string
  /** 控件的 id。提供后标题渲染为 `<label for>`；开关等不需要时省略并改用 `aria-labelledby`。 */
  labelFor?: string
  /** 一行必要说明：风险或数据流向、约束、禁用原因。可选解释用 `hint`。 */
  description?: string
  /** 说明下方的补充内容，用于带入口的禁用原因等需要链接的必要说明；纯文本说明用 `description`。 */
  note?: Snippet
  /** 可选解释（默认值、单位、示例）。只在标题右侧问号提示中出现，任何宽度一致。 */
  hint?: string
  /** 控件前的等宽 muted 只读值，如版本号、地址、数据量。 */
  value?: string
  /** 控件语义宽度。宽行中控件列取该宽度并贴右缘；窄行中控件占满整行。 */
  controlSize?: FieldControlSize
  /**
   * `auto`（默认）：行宽低于 50rem 时标题在上、控件占满宽度；`inline`：任何宽度都左右排布，用于开关与只读值。
   */
  layout?: 'auto' | 'inline'
  /** 依赖父开关的子设置：紧跟父行，以左侧竖线缩进一级。父开关关闭时由调用方不渲染。 */
  nested?: boolean
  /** 提供后在行尾显示「编辑」，点击打开 Sheet 等显式保存表面；与只读 `value` 摘要配合使用。 */
  onedit?: () => void
  /** 右侧控件、只读值或动作。可用 `{#snippet children({ titleId, descriptionId })}` 取得关联 id。 */
  children?: Snippet<[SettingRowIds]>
  class?: string
}
</script>

<script lang="ts">
import * as m from '#lib/paraglide/messages.js'

import { Button } from '#lib/components/ui/button/index.js'
import { fieldControlWidths } from '#lib/components/ui/field/field-sizes.js'
import FieldHelp from '#lib/components/ui/field/field-help.svelte'
import { Label } from '#lib/components/ui/label/index.js'
import { cn } from '#lib/utils.js'

let {
  title,
  labelFor,
  description,
  note,
  hint,
  value,
  controlSize,
  layout = 'auto',
  nested = false,
  onedit,
  children,
  class: className,
}: SettingRowProps = $props()

const id = $props.id()
const titleId = `${id}-title`
const editId = `${id}-edit`
const descriptionId = $derived(description ? `${id}-description` : undefined)
</script>

<!--
  设置行：左侧标题（可选一行必要说明与问号提示），右侧控件或只读值；行宽低于 50rem 时标题在上、控件占满宽度（layout="inline" 例外）。
  行之间 1px 分隔线，首行之上不画线；以 60rem 设置列为上限，左对齐。
-->
<div
  data-slot="setting-row"
  data-layout={layout}
  data-nested={nested ? 'true' : undefined}
  class={cn(
    '@container/setting-row w-full min-w-0 max-w-[60rem] py-3',
    '[[data-slot=setting-row]+&]:border-t',
    nested && 'ms-2 w-auto max-w-[calc(60rem-0.5rem)] border-s ps-4',
    className,
  )}>
  <div
    class={cn(
      'flex min-w-0 gap-2',
      layout === 'inline'
        ? 'items-start justify-between gap-4'
        : 'flex-col @min-[50rem]/setting-row:flex-row @min-[50rem]/setting-row:items-start @min-[50rem]/setting-row:justify-between @min-[50rem]/setting-row:gap-8',
    )}>
    <div class="flex min-w-0 flex-1 flex-col gap-0.5">
      <div class="flex min-w-0 items-center gap-2">
        {#if labelFor}
          <Label id={titleId} for={labelFor} class="min-w-0 text-sm leading-snug font-medium">{title}</Label>
        {:else}
          <span id={titleId} class="min-w-0 text-sm leading-snug font-medium">{title}</span>
        {/if}
        {#if hint}<FieldHelp {hint} />{/if}
      </div>
      {#if description}
        <p id={descriptionId} class="text-sm leading-normal text-muted-foreground">{description}</p>
      {/if}
      {@render note?.()}
    </div>
    {#if children || value !== undefined || onedit}
      <div
        data-slot="setting-row-control"
        style:--setting-control-width={controlSize ? fieldControlWidths[controlSize] : undefined}
        class={cn(
          'flex min-w-0 items-center gap-3',
          layout === 'inline'
            ? 'shrink-0 justify-end'
            : 'w-full @min-[50rem]/setting-row:w-auto @min-[50rem]/setting-row:shrink-0 @min-[50rem]/setting-row:justify-end',
          controlSize && layout !== 'inline' && '@min-[50rem]/setting-row:w-(--setting-control-width)',
        )}>
        {#if value !== undefined}
          <span class="min-w-0 truncate font-technical text-xs text-muted-foreground tabular-nums">{value}</span>
        {/if}
        {@render children?.({ titleId, descriptionId })}
        {#if onedit}
          <Button
            id={editId}
            type="button"
            variant="outline"
            size="sm"
            aria-labelledby="{editId} {titleId}"
            onclick={onedit}>{m.common_edit()}</Button>
        {/if}
      </div>
    {/if}
  </div>
</div>
