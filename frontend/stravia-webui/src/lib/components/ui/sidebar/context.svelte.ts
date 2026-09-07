import { createContext } from 'svelte'

type Getter<T> = () => T

export type SidebarStateProps = {
  open: Getter<boolean>
  setOpen: (open: boolean) => void
  openMobile: Getter<boolean>
  setOpenMobile: (open: boolean) => void
  isMobile: Getter<boolean>
}

class SidebarState {
  readonly props: SidebarStateProps
  open = $derived.by(() => this.props.open())
  openMobile = $derived.by(() => this.props.openMobile())
  isMobile = $derived.by(() => this.props.isMobile())
  state = $derived.by(() => (this.open ? 'expanded' : 'collapsed'))

  constructor(props: SidebarStateProps) {
    this.props = props
  }

  setOpen = (value: boolean) => this.props.setOpen(value)
  setOpenMobile = (value: boolean) => this.props.setOpenMobile(value)
  toggle = () => (this.isMobile ? this.setOpenMobile(!this.openMobile) : this.setOpen(!this.open))
}

const [getSidebar, provideSidebar] = createContext<SidebarState>()

export function setSidebar(props: SidebarStateProps): SidebarState {
  return provideSidebar(new SidebarState(props))
}

export function useSidebar(): SidebarState {
  return getSidebar()
}
