import { cubicOut } from 'svelte/easing'
import { prefersReducedMotion, Tween } from 'svelte/motion'

/** 数据切换时图形补间的时长；先快后慢、不回弹，符合 DESIGN.md 对动效“快速、可预测”的要求。 */
export const CHART_MOTION_MS = 300
/** 悬停高亮属于控件反馈，沿用 DESIGN.md 的 140ms。 */
export const CHART_FOCUS_MS = 140

/**
 * layerchart 组件的 `motion` 属性。减少动态效果时关闭补间。
 * layerchart 只在组件创建时读取该配置，系统偏好在运行中切换要到下一次挂载才生效。
 */
export function chartMotion() {
  return prefersReducedMotion.current
    ? ({ type: 'none' } as const)
    : ({ type: 'tween', duration: CHART_MOTION_MS, easing: cubicOut } as const)
}

/**
 * 返回一个跟随 `read()` 结果缓动的 Tween；须在组件初始化期间调用。
 * 初始值取创建时的 `read()`，之后每次变化都从当前位置缓动到新目标，每次读取时再判断系统的减少动态效果偏好。
 */
export function followTween<T extends number[]>(read: () => T, duration = CHART_MOTION_MS): Tween<T> {
  const tween = new Tween(read())
  $effect.pre(() => {
    void tween.set(read(), { duration: prefersReducedMotion.current ? 0 : duration, easing: cubicOut })
  })
  return tween
}
