import { createContext } from 'svelte'

/** One observer per observation log, shared only by its expensive body blocks. */
export interface ObservationBodyViewport {
  observe: (element: HTMLElement, update: (near: boolean) => void) => () => void
  root: () => HTMLElement | undefined
  resizeAbove: (delta: number) => void
}

export const [getObservationBodyViewport, setObservationBodyViewport] = createContext<ObservationBodyViewport>()
