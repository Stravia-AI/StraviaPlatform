import { beforeNavigate, goto } from '$app/navigation'
import { resolve } from '$app/paths'
import { untrack } from 'svelte'
import type { Path } from '$app/types'
import { DraftGuardController, type DraftGuardDecision, type DraftGuardState } from '#lib/draft-guard.js'

/**
 * 资源草稿的离开保护（Svelte 壳）。必须在组件初始化时创建，因为它注册 `beforeNavigate`。
 *
 * ```svelte
 * const guard = new DraftGuard({ dirty: () => baseline.dirty, busy: () => saving })
 *
 * <Sheet.Root bind:open={() => open, guard.openChange((next) => (open = next))}>
 *   <Sheet.Content locked={guard.busy}>…<Button onclick={() => guard.request(close)}>取消</Button>
 * </Sheet.Root>
 * <DraftDiscardDialog {guard} />
 * ```
 *
 * Escape、遮罩与关闭按钮都会触发 Sheet/Dialog 的 `onOpenChange(false)`，因此只需把
 * `openChange` 接到 Root 上；取消按钮、返回等自有出口调用 `request`；路由离开由本类自动拦截。
 * 刷新与关页：SvelteKit 对卸载同样调用 `beforeNavigate`，守卫 cancel 后由浏览器给出原生提示。
 */
export class DraftGuard {
  /** 确认对话框是否应该显示；由 `DraftDiscardDialog` 读取。 */
  confirming = $state(false)

  readonly #state: DraftGuardState
  readonly #controller: DraftGuardController

  constructor(state: DraftGuardState) {
    this.#state = state
    this.#controller = new DraftGuardController(state, (confirming) => {
      this.confirming = confirming
    })

    beforeNavigate((navigation) => {
      // 同页 shallow goto（例如更新 query）不是离开。
      if (navigation.shallow && navigation.type === 'goto') return
      const { to } = navigation
      let leave: (() => unknown) | undefined
      if (navigation.type === 'popstate') {
        const delta = navigation.delta
        leave = delta === undefined ? undefined : () => window.history.go(delta)
      } else if (to) {
        const { pathname, search, hash } = to.url
        const destination = resolve(`${pathname.slice(resolve('').length)}${search}${hash}` as Path)
        leave = () => goto(destination)
      }
      this.#controller.guardNavigation(navigation, leave)
    })
  }

  get busy(): boolean {
    return this.#state.busy()
  }

  /** 把会丢失草稿的动作交给守卫；返回守卫的决定。 */
  request = (action: () => unknown): DraftGuardDecision => this.#controller.request(action)

  /**
   * 包装 Root 的 `onOpenChange`：打开直接放行，关闭经过守卫。`apply` 负责真正改变 open 并重置草稿。
   * 配合函数绑定使用：`bind:open={() => open, guard.openChange((next) => (open = next))}`。
   */
  openChange =
    (apply: (open: boolean) => unknown) =>
    (open: boolean): void => {
      if (open) apply(true)
      else this.request(() => apply(false))
    }

  confirmDiscard = (): Promise<void> => this.#controller.confirmDiscard()
  keepEditing = (): void => this.#controller.keepEditing()
}

/**
 * 草稿基线：`read()` 返回草稿的可序列化快照，`dirty` 表示当前快照与基线不同。
 * 打开编辑器或保存成功后调用 `reset()` 把当前草稿记为基线。
 */
export class DraftBaseline<T> {
  readonly #read: () => T
  #saved = $state.raw('')

  constructor(read: () => T) {
    this.#read = read
    this.#saved = untrack(() => JSON.stringify(read()))
  }

  get dirty(): boolean {
    return JSON.stringify(this.#read()) !== this.#saved
  }

  reset(): void {
    this.#saved = untrack(() => JSON.stringify(this.#read()))
  }
}
