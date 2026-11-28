/**
 * 统一指针拖拽引擎（鼠标 / 触屏 / 触控笔共用一套 Pointer Events 链路）。
 *
 * 相比原生 HTML5 Drag-and-Drop：
 * - 不触发跨浏览器差异的 `dropEffect` 光标或长按冲突；
 * - 拖拽起手与落点高亮通过固定槽位 + `data-drag-over` 渲染，零布局跳动；
 * - 支持位移阈值（6px 內按普通点击处理）、悬浮跟手预览、`Escape` 取消，以及与 `drag-autoscroll.ts` 协同滚屏。
 */

/** 指针位移超过该距离才正式进入拖拽；以内松手保持普通点击行为。 */
export const POINTER_DRAG_SLOP_PX = 6

/** 指针当前命中的落点元素带上该属性，供样式高亮。 */
const DRAG_OVER_ATTRIBUTE = 'data-drag-over'

export interface PointerDragOptions {
  /** 被拖拽的源元素；进入拖拽时克隆为悬浮跟手预览。 */
  source: HTMLElement
  /** 合法落点选择器；每帧取指针下最近的匹配祖先。 */
  zoneSelector: string
  /** 位移超过阈值，拖拽正式开始。 */
  onStart: () => void
  /** 松手落定；`zone` 为指针下的落点元素，不在合法落点上时为 `null`。 */
  onDrop: (zone: Element | null) => void | Promise<void>
  /** 用户按 Escape 或系统取消手势时中断拖拽。 */
  onCancel: () => void
}

interface Point {
  x: number
  y: number
}

let active = false

/** 当前是否存在进行中的指针拖拽（含按下尚未超过位移阈值阶段）。 */
export function pointerDragInProgress(): boolean {
  return active
}

function createPreview(source: HTMLElement, rect: DOMRect): HTMLElement {
  const preview = source.cloneNode(true) as HTMLElement
  const stripped = [
    'data-slot',
    'data-key',
    'data-enabled',
    'data-model',
    'data-priority',
    'data-config-mode',
    'role',
    'tabindex',
    'aria-label',
    'id',
  ]
  for (const element of [preview, ...preview.querySelectorAll('*')]) {
    for (const attribute of stripped) {
      element.removeAttribute(attribute)
    }
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

/** 吞掉拖拽结束松手后浏览器可能补发的那次 click，防止误打开抽屉或弹窗。 */
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
  window.addEventListener('pointerdown', release, true)
}

/**
 * 在拖拽手柄或可拖元素的 `pointerdown` 中调用。
 * 忽略右键/中键、非主指针以及已在进行的拖拽；结束后自动清理所有监听与预览。
 */
export function beginPointerDrag(event: PointerEvent, options: PointerDragOptions): void {
  if (!event.isPrimary || active) return
  if (event.pointerType === 'mouse' && event.button !== 0) return
  active = true

  const { source, zoneSelector } = options
  const pointerId = event.pointerId
  const origin: Point = { x: event.clientX, y: event.clientY }
  const sourceRect = source.getBoundingClientRect()
  const grab: Point = { x: origin.x - sourceRect.left, y: origin.y - sourceRect.top }
  let pointer = origin
  let preview: HTMLElement | undefined
  let hovered: Element | null = null
  let previousUserSelect = ''

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
    window.removeEventListener('keydown', onKeyDown, true)
    document.removeEventListener('scroll', highlight, true)
    hovered?.removeAttribute(DRAG_OVER_ATTRIBUTE)
    if (preview) {
      document.body.style.userSelect = previousUserSelect
      preview.remove()
    }
    active = false
  }

  const move = (moveEvent: PointerEvent) => {
    if (moveEvent.pointerId !== pointerId) return
    pointer = { x: moveEvent.clientX, y: moveEvent.clientY }
    if (!preview) {
      if (Math.hypot(pointer.x - origin.x, pointer.y - origin.y) < POINTER_DRAG_SLOP_PX) return
      previousUserSelect = document.body.style.userSelect
      document.body.style.userSelect = 'none'
      window.getSelection()?.removeAllRanges()
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

  const onKeyDown = (keyEvent: KeyboardEvent) => {
    if (keyEvent.key !== 'Escape') return
    keyEvent.preventDefault()
    keyEvent.stopPropagation()
    const dragged = preview !== undefined
    stop()
    if (dragged) options.onCancel()
  }

  window.addEventListener('pointermove', move)
  window.addEventListener('pointerup', up)
  window.addEventListener('pointercancel', cancel)
  window.addEventListener('keydown', onKeyDown, true)
  // 指针悬停触发自动滚动时，指针下的落点元素也在持续变化。
  document.addEventListener('scroll', highlight, { capture: true, passive: true })
}
