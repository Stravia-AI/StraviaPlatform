/**
 * 资源草稿离开保护的纯逻辑。
 *
 * 契约：编辑器（Sheet、对话框、独立编辑页）提供两个事实——`dirty()` 有未保存变更、`busy()` 正在保存；
 * 所有会丢失草稿的出口（Escape、遮罩、关闭按钮、取消、返回、路由离开）都通过 `request()` 或
 * `guardNavigation()` 进入：
 *
 * - busy：一律拒绝（保存中锁定提交、编辑与关闭）；
 * - 无草稿：立即执行；
 * - 有草稿：挂起动作并进入确认，`confirmDiscard()` 才执行，`keepEditing()` 丢弃动作、草稿原样保留。
 *
 * 保存失败不改变 dirty，草稿因此留在编辑器里；保存成功由调用方重置基线后程序化关闭，不经过守卫。
 */
export interface DraftGuardState {
  dirty(): boolean
  busy(): boolean
}

export type DraftGuardDecision = 'run' | 'confirm' | 'blocked'

export interface DraftGuardNavigation {
  /** 页面卸载（刷新、关页、外链）。此时只能 cancel 触发浏览器原生提示，无法渲染自己的确认。 */
  willUnload: boolean
  cancel(): void
}

type Action = () => unknown

export class DraftGuardController {
  readonly #state: DraftGuardState
  readonly #onChange: (confirming: boolean) => void
  #pending: Action | undefined
  #allowNextNavigation = false

  constructor(state: DraftGuardState, onChange: (confirming: boolean) => void = () => undefined) {
    this.#state = state
    this.#onChange = onChange
  }

  get confirming(): boolean {
    return this.#pending !== undefined
  }

  request(action: Action): DraftGuardDecision {
    if (this.#state.busy()) return 'blocked'
    if (!this.#state.dirty()) {
      action()
      return 'run'
    }
    this.#setPending(action)
    return 'confirm'
  }

  /** 执行被挂起的动作。先清除挂起状态，避免动作触发的二次关闭重复进入确认。 */
  async confirmDiscard(): Promise<void> {
    const action = this.#pending
    if (!action) return
    this.#setPending(undefined)
    await action()
  }

  keepEditing(): void {
    this.#setPending(undefined)
  }

  /**
   * 路由离开。`leave` 在用户确认后重新发起这次导航（goto 或 history.go）；
   * 它发起的导航会放行一次，不再被同一个守卫拦住。无法重放的导航（未提供 `leave`）只取消。
   */
  guardNavigation(navigation: DraftGuardNavigation, leave?: Action): void {
    if (this.#allowNextNavigation) {
      this.#allowNextNavigation = false
      return
    }
    if (this.#state.busy()) {
      navigation.cancel()
      return
    }
    if (!this.#state.dirty()) return

    navigation.cancel()
    if (navigation.willUnload || !leave) return
    this.#setPending(() => {
      this.#allowNextNavigation = true
      const result = leave()
      // goto 的 Promise 结束即导航结束；popstate 的 history.go 是异步事件，放行标记留给那次导航消费。
      if (result instanceof Promise) {
        return result.finally(() => {
          this.#allowNextNavigation = false
        })
      }
      return result
    })
  }

  #setPending(action: Action | undefined): void {
    this.#pending = action
    this.#onChange(action !== undefined)
  }
}
