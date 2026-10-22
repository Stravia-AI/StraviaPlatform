/** 行内保存（单个文本或数字）的纯决策：键盘语义与外部已保存值的协调。组件见 `inline-setting-input.svelte`。 */

export type InlineSaveKeyAction = 'save' | 'revert' | null

/** Enter 保存、Escape 还原；无改动、保存中或输入法组合期间不响应。失焦不属于任何动作。 */
export function inlineSaveKeyAction(
  event: { key: string; isComposing: boolean },
  state: { dirty: boolean; saving: boolean },
): InlineSaveKeyAction {
  if (event.isComposing || !state.dirty || state.saving) return null
  if (event.key === 'Enter') return 'save'
  if (event.key === 'Escape') return 'revert'
  return null
}

/** 已保存值被外部更新时：未编辑的草稿跟随新值，已编辑的草稿保持不变。 */
export function reconcileInlineDraft(input: { draft: string; previousSaved: string; nextSaved: string }): string {
  return input.draft === input.previousSaved ? input.nextSaved : input.draft
}
