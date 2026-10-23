import { getContext, setContext, untrack } from 'svelte'
import { SvelteSet } from 'svelte/reactivity'

/**
 * 页面内行内保存输入的未保存草稿登记。页面提供一个登记表并把它交给 `DraftGuard`，
 * 其下每个 `InlineSettingInput` 在草稿与已保存值不同时登记自己，离开页面或切换分组前据此确认。
 */
export class InlineDraftRegistry {
  readonly #dirty = new SvelteSet<symbol>()

  get dirty(): boolean {
    return this.#dirty.size > 0
  }

  mark(token: symbol, dirty: boolean): void {
    untrack(() => {
      if (dirty) this.#dirty.add(token)
      else this.#dirty.delete(token)
    })
  }

  release(token: symbol): void {
    this.mark(token, false)
  }
}

const key = Symbol('inline-draft-registry')

export function provideInlineDraftRegistry(): InlineDraftRegistry {
  const registry = new InlineDraftRegistry()
  setContext(key, registry)
  return registry
}

export function useInlineDraftRegistry(): InlineDraftRegistry | undefined {
  return getContext<InlineDraftRegistry | undefined>(key)
}
