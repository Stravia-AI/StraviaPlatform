<script lang="ts" module>
export interface InlineSettingInputProps {
  /** 已保存值（可绑定）。成功保存后更新为 `onSave` 返回的规范值，未返回则为提交值。 */
  value: string
  /**
   * 保存回调。成功 resolve（可返回服务端规范化后的字符串）；失败 throw / reject：草稿原样保留，
   * 行内显示 `localizeBackendErrorMessage` 后的原因，用户可修改后重试或取消。
   */
  onSave: (next: string) => void | string | Promise<void | string>
  /** 提交前的本地校验，返回错误文案则不调用 `onSave`。 */
  validate?: (draft: string) => string | null | undefined
  id?: string
  type?: 'text' | 'number' | 'url'
  inputmode?: 'text' | 'numeric' | 'decimal' | 'url'
  min?: number | string
  max?: number | string
  step?: number | string
  placeholder?: string
  autocomplete?: 'off' | 'on'
  /** 无可见 label 时的可访问名称；与 `labelledby` 二选一，在设置行中优先用 `labelledby`。 */
  label?: string
  /** 关联设置行标题，取自 `SettingRow` 的 `children` 参数。 */
  labelledby?: string
  describedby?: string
  disabled?: boolean
  /** 等宽字体，用于 URL、标识等技术值。 */
  technical?: boolean
  class?: string
}
</script>

<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { onDestroy, untrack } from 'svelte'

import { localizeBackendErrorMessage } from '#lib/backend-error.js'
import { Button } from '#lib/components/ui/button/index.js'
import { Input } from '#lib/components/ui/input/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'
import { inlineSaveKeyAction, reconcileInlineDraft } from '#lib/inline-save.js'
import { cn } from '#lib/utils.js'

let {
  value = $bindable(),
  onSave,
  validate,
  id,
  type = 'text',
  inputmode,
  min,
  max,
  step,
  placeholder,
  autocomplete = 'off',
  label,
  labelledby,
  describedby,
  disabled = false,
  technical = false,
  class: className,
}: InlineSettingInputProps = $props()

const errorId = $props.id()
const SAVED_SIGNAL_MS = 2400

let draft = $state(untrack(() => value))
let trackedSaved = untrack(() => value)
let saving = $state(false)
let error = $state('')
let savedSignal = $state(false)
let signalTimer: ReturnType<typeof setTimeout> | undefined

const dirty = $derived(draft !== value)

// 外部更新已保存值（重新加载、其他表面写入）时，未编辑的草稿跟随；已编辑的草稿保留。
$effect.pre(() => {
  const next = value
  if (next === trackedSaved) return
  untrack(() => {
    draft = reconcileInlineDraft({ draft, previousSaved: trackedSaved, nextSaved: next })
  })
  trackedSaved = next
})

onDestroy(() => clearTimeout(signalTimer))

function revert() {
  draft = value
  error = ''
}

async function save() {
  if (saving || !dirty) return
  const invalid = validate?.(draft)
  if (invalid) {
    error = invalid
    return
  }
  const submitted = draft
  saving = true
  error = ''
  savedSignal = false
  clearTimeout(signalTimer)
  try {
    const result = await onSave(submitted)
    const saved = typeof result === 'string' ? result : submitted
    trackedSaved = saved
    value = saved
    draft = saved
    savedSignal = true
    signalTimer = setTimeout(() => (savedSignal = false), SAVED_SIGNAL_MS)
  } catch (cause) {
    error = localizeBackendErrorMessage(cause)
  } finally {
    saving = false
  }
}

function onkeydown(event: KeyboardEvent) {
  const action = inlineSaveKeyAction(event, { dirty, saving })
  if (!action) return
  // Escape 仅还原本行草稿，不再关闭外层 Sheet 或对话框。
  event.preventDefault()
  event.stopPropagation()
  if (action === 'save') void save()
  else revert()
}
</script>

<!-- 失焦不提交：只有 Enter 或「保存」才写入。有改动时出现「保存」「取消」；成功后短暂显示 signal「已保存」。 -->
<div data-slot="inline-setting-input" class={cn('flex w-full min-w-0 flex-col gap-1', className)}>
  <div class="flex min-w-0 flex-wrap items-center justify-end gap-2">
    <Input
      {id}
      {type}
      {inputmode}
      {min}
      {max}
      {step}
      {placeholder}
      {autocomplete}
      aria-label={label}
      aria-labelledby={labelledby}
      aria-describedby={cn(describedby, error && errorId) || undefined}
      aria-invalid={error ? true : undefined}
      aria-busy={saving || undefined}
      disabled={disabled || saving}
      class={cn(
        'w-full @min-[50rem]/setting-row:w-(--setting-control-width,100%) @min-[50rem]/setting-row:min-w-0',
        technical && 'font-technical',
      )}
      value={draft}
      oninput={(event: Event) => (draft = (event.currentTarget as HTMLInputElement).value)}
      {onkeydown} />
    {#if dirty || saving}
      <div class="flex shrink-0 items-center gap-2">
        <Button
          type="button"
          size="sm"
          disabled={disabled || saving}
          aria-busy={saving || undefined}
          onclick={() => void save()}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{m.common_save()}
        </Button>
        <Button type="button" size="sm" variant="ghost" disabled={saving} onclick={revert}>{m.common_cancel()}</Button>
      </div>
    {/if}
    {#if savedSignal && !dirty}
      <span role="status" class="shrink-0 text-sm text-signal">{m.common_saved()}</span>
    {/if}
  </div>
  {#if error}
    <p id={errorId} role="alert" class="text-sm text-destructive">{error}</p>
  {/if}
</div>
