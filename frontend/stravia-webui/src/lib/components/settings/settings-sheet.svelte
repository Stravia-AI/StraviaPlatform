<script lang="ts" module>
import type { Snippet } from 'svelte'

export interface SettingsSheetProps {
  open: boolean
  /** 任务标题，如「编辑代理」。 */
  title: string
  /** 保存按钮文字。 */
  submitLabel: string
  /** 草稿的可序列化快照；打开时记为基线，与基线不同即有未保存草稿，关闭前需确认放弃。 */
  draft: () => unknown
  /** 保存中：锁定提交、编辑与关闭。 */
  saving?: boolean
  /** 无可保存的改动时禁用保存。 */
  submitDisabled?: boolean
  /** 保存失败原因；Sheet 与输入保持打开。 */
  error?: string
  /** 保存按钮旁的影响说明，如「保存后所有会话都会退出」。 */
  footerNote?: string
  /** 提交：成功后由调用方关闭 Sheet。 */
  onsubmit: () => void | Promise<void>
  /** 关闭（含取消、遮罩、Escape）后的回调，用于丢弃草稿。 */
  onclose?: () => void
  children: Snippet
}
</script>

<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { untrack } from 'svelte'

import DraftDiscardDialog from '#lib/components/draft-discard-dialog.svelte'
import { DraftBaseline, DraftGuard } from '#lib/draft-guard.svelte.js'
import * as Alert from '#lib/components/ui/alert/index.js'
import { Button } from '#lib/components/ui/button/index.js'
import * as Sheet from '#lib/components/ui/sheet/index.js'
import { Spinner } from '#lib/components/ui/spinner/index.js'

let {
  open = $bindable(),
  title,
  draft,
  submitLabel,
  saving = false,
  submitDisabled = false,
  error = '',
  footerNote,
  onsubmit,
  onclose,
  children,
}: SettingsSheetProps = $props()

const noteId = $props.id()

const baseline = new DraftBaseline(() => draft())
const guard = new DraftGuard({ dirty: () => open && baseline.dirty, busy: () => saving })

// 调用方先写好草稿再打开，因此打开时的快照就是基线；保存成功后由调用方直接关闭，不经过守卫。
$effect.pre(() => {
  if (open) untrack(() => baseline.reset())
})

function apply(next: boolean): void {
  open = next
  if (!next) onclose?.()
}
</script>

<!--
  设置页的显式保存表面：只读摘要的「编辑」与需要补齐字段的开关都打开它。
  取消、遮罩与 Escape 都等同于取消，保持原值；保存中锁定关闭。
-->
<Sheet.Root bind:open={() => open, guard.openChange(apply)}>
  <Sheet.Content
    side="right"
    class="route-overlay-content w-full! gap-0 overflow-hidden p-0 sm:max-w-[32rem]!"
    closeLabel={m.common_close()}
    locked={guard.busy}>
    <Sheet.Header class="border-b pr-12">
      <Sheet.Title>{title}</Sheet.Title>
    </Sheet.Header>
    <form
      class="route-overlay-form"
      onsubmit={(event) => {
        event.preventDefault()
        if (!saving) void onsubmit()
      }}>
      <div class="route-overlay-body flex flex-col gap-4">
        {@render children()}
        {#if error}
          <Alert.Root variant="destructive" role="alert">
            <Alert.Description>{error}</Alert.Description>
          </Alert.Root>
        {/if}
      </div>
      <Sheet.Footer class="route-overlay-footer pb-[max(0.75rem,env(safe-area-inset-bottom))]">
        {#if footerNote}<p id={noteId} class="me-auto min-w-0 flex-1 text-sm text-muted-foreground">
            {footerNote}
          </p>{/if}
        <Button type="button" variant="outline" disabled={saving} onclick={() => guard.request(() => apply(false))}>
          {m.common_cancel()}
        </Button>
        <Button
          type="submit"
          disabled={saving || submitDisabled}
          aria-busy={saving}
          aria-describedby={footerNote ? noteId : undefined}>
          {#if saving}<Spinner data-icon="inline-start" />{/if}{submitLabel}
        </Button>
      </Sheet.Footer>
    </form>
  </Sheet.Content>
</Sheet.Root>

<DraftDiscardDialog {guard} />
