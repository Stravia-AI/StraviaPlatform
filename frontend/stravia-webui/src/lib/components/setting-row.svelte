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
  /** 控件语义宽度（number、setting、datetime）。并排时控件列取该宽度并贴右缘；上下排布时控件占满整行。自由文本控件不设宽度，改用 `layout="stacked"`。 */
  controlSize?: FieldControlSize
  /**
   * `auto`（默认）：行宽低于 32rem 时标题在上、控件占满宽度，否则左右并排；
   * `inline`：任何宽度都左右排布，用于开关与只读值；
   * `stacked`：任何宽度都标题在上、控件占满整行，用于名称、地址、密钥等自由文本。
   */
  layout?: 'auto' | 'inline' | 'stacked'
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
import TechnicalValue from '#lib/components/technical-value.svelte'
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
  设置行：左侧标题（可选一行必要说明与问号提示），右侧控件或只读值；行宽低于 32rem 时标题在上、控件占满宽度（layout="inline" 例外），
  自由文本控件用 layout="stacked" 始终上下排布。行之间 1px 分隔线，首行之上不画线；宽度由所在设置列决定。
-->
<div
  data-slot="setting-row"
  data-layout={layout}
  data-nested={nested ? 'true' : undefined}
  class={cn(
    // 单行设置保持 44px 的可点行高：标题行高 19.25px 加 24px 内边距只有 43.25px，低于触控与可读下限。
    '@container/setting-row w-full min-w-0 py-3 min-h-11',
    '[[data-slot=setting-row]+&]:border-t',
    nested && 'ms-2 w-auto border-s ps-4',
    className,
  )}>
  <div
    class={cn(
      'flex min-w-0 gap-2',
      layout === 'inline'
        ? ['justify-between gap-4', description || note ? 'items-start' : 'items-center']
        : layout === 'stacked'
          ? 'flex-col'
          : [
              'flex-col @min-[32rem]/setting-row:flex-row @min-[32rem]/setting-row:justify-between @min-[32rem]/setting-row:gap-8',
              description || note ? '@min-[32rem]/setting-row:items-start' : '@min-[32rem]/setting-row:items-center',
            ],
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
        <p id={descriptionId} class="text-xs leading-normal text-muted-foreground">{description}</p>
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
            : layout === 'stacked'
              ? 'w-full'
              : 'w-full @min-[32rem]/setting-row:w-auto @min-[32rem]/setting-row:shrink-0 @min-[32rem]/setting-row:justify-end',
          controlSize && layout === 'auto' && '@min-[32rem]/setting-row:w-(--setting-control-width)',
        )}>
        {#if value !== undefined}
          <TechnicalValue {value} class="text-muted-foreground" />
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
