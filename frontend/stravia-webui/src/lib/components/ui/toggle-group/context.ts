import { getContext, setContext } from 'svelte'

import type { ToggleVariants } from '$lib/components/ui/toggle/index.js'

export interface ToggleGroupContext extends ToggleVariants {
  spacing?: number
  orientation?: 'horizontal' | 'vertical'
}

export function setToggleGroupCtx(props: ToggleGroupContext) {
  setContext('toggleGroup', props)
}

export function getToggleGroupCtx() {
  return getContext<Required<ToggleGroupContext>>('toggleGroup')
}
