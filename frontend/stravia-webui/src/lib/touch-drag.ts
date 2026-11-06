/**
 * 触屏拖拽。
 *
 * HTML5 拖放在移动浏览器上要么不触发，要么依赖各引擎不一的长按手势，且拖动时页面会随手指滚动。
 * 这里改用 Pointer Events 驱动：手柄元素声明 `touch-action: none`，按下后移动超过阈值即开始拖拽，
 * 用悬浮预览跟手，松手时按指针下的落点元素交给调用方处理。鼠标仍走原生 HTML5 拖放。
 */

/** 手指位移超过该距离才算拖拽；以内松手仍是普通点按，保留点击打开编辑器的行为。 */
export const TOUCH_DRAG_SLOP_PX = 6

/** 指针下的落点元素带上该属性，供样式高亮。 */
const DRAG_OVER_ATTRIBUTE = 'data-drag-over'

export interface TouchDragOptions {
  /** 被拖拽的元素；拖拽开始时复制成跟手预览。 */
  source: HTMLElement
  /** 落点候选的选择器；取指针下最近的匹配祖先。 */
  zoneSelector: string
  /** 位移超过阈值，拖拽真正开始。 */
  onStart: () => void
  /** 松手；`zone` 为指针下的落点元素，不在任何落点上时为 `null`。 */
  onDrop: (zone: Element | null) => void | Promise<void>
  /** 系统取消手势（如来电、系统手势）导致拖拽中断。 */
  onCancel: () => void
}

interface Point {
  x: number
  y: number
}

let active = false

/** 是否有触屏手势正在拖拽手柄（含尚未超过阈值的按下）。 */
export function touchDragInProgress(): boolean {
  return active
}

function createPreview(source: HTMLElement, rect: DOMRect): HTMLElement {
  const preview = source.cloneNode(true) as HTMLElement
  // 预览只是视觉副本：去掉能被查询、聚焦或读屏识别为第二张卡片的属性。
  for (const attribute of ['data-slot', 'data-key', 'data-enabled', 'role', 'tabindex', 'aria-label', 'id']) {
    preview.removeAttribute(attribute)
  }
  preview.setAttribute('aria-hidden', 'true')
  preview.toggleAttribute('inert', true)
  Object.assign(preview.style, {
    position: 'fixed',
    left: '0',
    top: '0',
    width: `${rect.width}px`,
    height: `${rect.height}px`,
    margin: '0',
    zIndex: '100',
    pointerEvents: 'none',
    opacity: '0.92',
    boxShadow: '0 12px 32px rgb(0 0 0 / 0.28)',
    transition: 'none',
    willChange: 'transform',
  })
  return preview
}

/** 吞掉拖拽结束后浏览器可能补发的那次 click，免得卡片被误当作点按打开编辑器。 */
function swallowNextClick(): void {
  const release = () => {
    window.removeEventListener('click', swallow, true)
    window.removeEventListener('pointerdown', release, true)
  }
  const swallow = (event: Event) => {
    event.stopPropagation()
    event.preventDefault()
    release()
  }
  window.addEventListener('click', swallow, true)
  // 下一次按下必在下一次 click 之前；没有补发 click 时由它撤销拦截。
  window.addEventListener('pointerdown', release, true)
}

/**
 * 在手柄的 `pointerdown` 中调用。鼠标按下或已有拖拽进行时忽略，由原生拖放接管。
 * 手势结束（松手或取消）后自动清理监听与预览。
 */
export function beginTouchDrag(event: PointerEvent, options: TouchDragOptions): void {
  if (event.pointerType === 'mouse' || !event.isPrimary || active) return
  active = true

  const { source, zoneSelector } = options
  const pointerId = event.pointerId
  const origin: Point = { x: event.clientX, y: event.clientY }
  const sourceRect = source.getBoundingClientRect()
  const grab: Point = { x: origin.x - sourceRect.left, y: origin.y - sourceRect.top }
  let pointer = origin
  let preview: HTMLElement | undefined
  let hovered: Element | null = null

  const zoneAt = (point: Point): Element | null =>
    document.elementFromPoint(point.x, point.y)?.closest(zoneSelector) ?? null

  const highlight = () => {
    const zone = zoneAt(pointer)
    if (zone === hovered) return
    hovered?.removeAttribute(DRAG_OVER_ATTRIBUTE)
    zone?.setAttribute(DRAG_OVER_ATTRIBUTE, 'true')
    hovered = zone
  }

  const stop = () => {
    window.removeEventListener('pointermove', move)
    window.removeEventListener('pointerup', up)
    window.removeEventListener('pointercancel', cancel)
    document.removeEventListener('scroll', highlight, true)
    hovered?.removeAttribute(DRAG_OVER_ATTRIBUTE)
    preview?.remove()
    active = false
  }

  const move = (moveEvent: PointerEvent) => {
    if (moveEvent.pointerId !== pointerId) return
    pointer = { x: moveEvent.clientX, y: moveEvent.clientY }
    if (!preview) {
      if (Math.hypot(pointer.x - origin.x, pointer.y - origin.y) < TOUCH_DRAG_SLOP_PX) return
      // 先复制再通知调用方：回调会把源卡片标成"拖拽中"，预览应保持原样。
      preview = createPreview(source, sourceRect)
      document.body.append(preview)
      options.onStart()
    }
    preview.style.transform = `translate3d(${pointer.x - grab.x}px, ${pointer.y - grab.y}px, 0)`
    highlight()
  }

  const up = (upEvent: PointerEvent) => {
    if (upEvent.pointerId !== pointerId) return
    pointer = { x: upEvent.clientX, y: upEvent.clientY }
    const dragged = preview !== undefined
    const zone = dragged ? zoneAt(pointer) : null
    stop()
    if (!dragged) return
    swallowNextClick()
    void options.onDrop(zone)
  }

  const cancel = (cancelEvent: PointerEvent) => {
    if (cancelEvent.pointerId !== pointerId) return
    const dragged = preview !== undefined
    stop()
    if (dragged) options.onCancel()
  }

  window.addEventListener('pointermove', move)
  window.addEventListener('pointerup', up)
  window.addEventListener('pointercancel', cancel)
  // 手指不动而页面自动滚动时，指针下的落点也会变。
  document.addEventListener('scroll', highlight, { capture: true, passive: true })
}
