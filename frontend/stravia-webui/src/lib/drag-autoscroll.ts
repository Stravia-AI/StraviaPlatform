/**
 * HTML5 拖拽期间让最近的可滚动祖先随指针滚动。
 *
 * 浏览器自带的拖拽自动滚动只在容器边缘极窄的一圈内生效，且各引擎（含桌面端 WebView）行为不一；
 * 管理页底部又有固定操作栏盖住这一圈，导致拖到页面外的目标无法到达。这里自行实现：边缘区内按距离线性加速。
 */

/** 距滚动容器上下边缘多少像素内开始滚动；需大于页面底部固定操作栏的可见高度，否则指针停在操作栏上就不会滚动。 */
export const AUTOSCROLL_EDGE_PX = 72
/** 指针贴着（或越过）边缘时的最大滚动速度，单位 px/ms。 */
export const AUTOSCROLL_MAX_SPEED = 0.9

/**
 * 指针在 `[top, bottom]` 内的纵向滚动速度（px/ms，向下为正）。
 * 指针越过边缘仍按最大速度计，便于拖出窗口边缘继续滚动。
 */
export function autoscrollSpeed(pointerY: number, top: number, bottom: number): number {
  // 边缘区不超过容器高度的四分之一，保证矮容器中间仍有不滚动的区域。
  const edge = Math.min(AUTOSCROLL_EDGE_PX, (bottom - top) / 4)
  if (edge <= 0) return 0
  if (pointerY < top + edge) return -AUTOSCROLL_MAX_SPEED * Math.min(1, (top + edge - pointerY) / edge)
  if (pointerY > bottom - edge) return AUTOSCROLL_MAX_SPEED * Math.min(1, (pointerY - (bottom - edge)) / edge)
  return 0
}

function canScrollVertically(element: Element): boolean {
  if (element.scrollHeight <= element.clientHeight) return false
  const { overflowY } = getComputedStyle(element)
  return overflowY === 'auto' || overflowY === 'scroll' || overflowY === 'overlay'
}

function scrollableAncestor(target: EventTarget | null): Element | null {
  for (let element = target instanceof Element ? target : null; element; element = element.parentElement) {
    if (element === document.documentElement) break
    if (canScrollVertically(element)) return element
  }
  const root = document.scrollingElement
  return root && root.scrollHeight > root.clientHeight ? root : null
}

function scrollportBounds(scroller: Element): { top: number; bottom: number } {
  if (scroller === document.scrollingElement) return { top: 0, bottom: window.innerHeight }
  const { top, bottom } = scroller.getBoundingClientRect()
  return { top, bottom }
}

/**
 * 开始跟随拖拽滚动，返回停止函数；拖拽结束时必须调用。
 * 指针停在边缘区内即使不再产生 `dragover` 也会持续滚动，直到停止。
 */
export function startDragAutoscroll(): () => void {
  let pointerY = 0
  let scroller: Element | null = null
  let lastFrame = 0
  let frame = 0
  // scrollTop 在部分显示缩放下会取整；累积余量避免低速时每帧不足 1px 而卡住。
  let carry = 0

  const track = (event: DragEvent) => {
    pointerY = event.clientY
    scroller = scrollableAncestor(event.target)
  }

  const step = (now: number) => {
    // 标签页退到后台后首帧间隔会很大，限幅避免恢复时跳一大段。
    const elapsed = lastFrame === 0 ? 0 : Math.min(now - lastFrame, 50)
    lastFrame = now
    if (scroller) {
      const { top, bottom } = scrollportBounds(scroller)
      carry += autoscrollSpeed(pointerY, top, bottom) * elapsed
      const whole = Math.trunc(carry)
      if (whole !== 0) {
        carry -= whole
        scroller.scrollBy({ top: whole, behavior: 'instant' })
      }
    }
    frame = requestAnimationFrame(step)
  }

  document.addEventListener('dragover', track, { capture: true, passive: true })
  frame = requestAnimationFrame(step)
  return () => {
    document.removeEventListener('dragover', track, { capture: true })
    cancelAnimationFrame(frame)
  }
}
