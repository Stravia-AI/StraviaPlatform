<script lang="ts">
import { untrack } from 'svelte'
import MarkdownContent from '$lib/components/markdown-content.svelte'

let {
  text,
  active = false,
  textStart = 0,
  snapshotKey = 0,
}: { text: string; active?: boolean; textStart?: number; snapshotKey?: number } = $props()

const segmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' })
// 与后端发布 100ms、工作区合并 100ms 共用 500ms 主动等待预算。
const REVEAL_BUDGET_MS = 300
let received = untrack(() => text)
let receivedStart = untrack(() => textStart)
let currentSnapshot = untrack(() => snapshotKey)
let displayed = $state(received)
let visibleEnd = received.length
let lastVisibleStart = segmenter.segment(received).containing(Math.max(0, visibleEnd - 1))?.index ?? 0
let pending: { start: number; end: number }[] = []
let cursor = 0
let frame: number | null = null
let deadlineTimer: ReturnType<typeof setTimeout> | null = null
let lastTime = 0
let credit = 0
let rate = 1 / 25
let deadline = 0
let reducedMotion = false

function cancelFrame() {
  if (frame !== null) cancelAnimationFrame(frame)
  if (deadlineTimer !== null) clearTimeout(deadlineTimer)
  frame = null
  deadlineTimer = null
  credit = 0
  lastTime = 0
  deadline = 0
}

function flush() {
  cancelFrame()
  pending = []
  cursor = 0
  visibleEnd = received.length
  lastVisibleStart = segmenter.segment(received).containing(Math.max(0, visibleEnd - 1))?.index ?? 0
  displayed = received
  rate = 1 / 25
}

function reveal(now: number) {
  frame = null
  if (now >= deadline) {
    flush()
    return
  }
  rate = Math.max(rate, (pending.length - cursor) / Math.max(1, deadline - lastTime))
  credit += (now - lastTime) * rate
  lastTime = now
  const count = Math.min(pending.length - cursor, Math.floor(credit))
  if (count > 0) {
    credit -= count
    cursor += count
    const boundary = pending[cursor - 1]
    visibleEnd = boundary.end
    lastVisibleStart = boundary.start
    displayed = received.slice(0, visibleEnd)
  }
  if (cursor < pending.length) frame = requestAnimationFrame(reveal)
  else {
    cancelFrame()
    pending = []
    cursor = 0
    rate = 1 / 25
  }
}

function synchronize(next: string, animate: boolean, snapshot: number, start: number) {
  const dropped = start - receivedStart
  const append =
    dropped === 0
      ? next.startsWith(received)
      : dropped > 0 &&
        dropped < received.length &&
        start + next.length > receivedStart + received.length &&
        next.startsWith(received.slice(dropped))
  const changed = next !== received || start !== receivedStart
  const initial = received.length === 0
  const restored = currentSnapshot !== snapshot
  currentSnapshot = snapshot
  received = next
  receivedStart = start
  if (!animate || reducedMotion || document.hidden || !append || initial || restored) {
    flush()
    return
  }
  if (!changed) return
  // 尾窗滑动只退役已裁掉的前缀，追加仍使用同一轮剩余预算。
  visibleEnd = Math.max(0, visibleEnd - dropped)
  lastVisibleStart = Math.max(0, lastVisibleStart - dropped)
  displayed = received.slice(0, visibleEnd)

  // 新片段可能延长末尾 grapheme；从该边界重新分段，避免拆开组合字符或 emoji。
  pending = []
  cursor = 0
  for (const segment of segmenter.segment(received.slice(lastVisibleStart))) {
    const start = lastVisibleStart + segment.index
    const end = start + segment.segment.length
    if (end <= visibleEnd) continue
    const lastUnit = received.charCodeAt(end - 1)
    if (end === received.length && lastUnit >= 0xd800 && lastUnit <= 0xdbff) continue
    if (start < visibleEnd) {
      visibleEnd = end
      displayed = received.slice(0, visibleEnd)
    } else {
      pending.push({ start, end })
    }
  }
  if (!pending.length) {
    cancelFrame()
    rate = 1 / 25
    return
  }

  // 持续追加不能延后本轮截止；大批正文用剩余预算自适应追赶。
  if (frame === null) {
    lastTime = performance.now()
    deadline = lastTime + REVEAL_BUDGET_MS
    // 帧边界可能晚于截止；固定 timer 收尾，不为持续追加重置预算。
    deadlineTimer = setTimeout(flush, REVEAL_BUDGET_MS)
    frame = requestAnimationFrame(reveal)
  }
  rate = Math.max(rate, pending.length / Math.max(1, deadline - performance.now()))
}

$effect(() => {
  const preference = window.matchMedia('(prefers-reduced-motion: reduce)')
  reducedMotion = preference.matches
  const update = () => {
    reducedMotion = preference.matches
    if (reducedMotion) flush()
  }
  preference.addEventListener('change', update)
  return () => {
    preference.removeEventListener('change', update)
    cancelFrame()
  }
})

$effect(() => {
  const next = text
  const animate = active
  const snapshot = snapshotKey
  const start = textStart
  // Only incoming props drive this effect, never the animation's own state.
  untrack(() => synchronize(next, animate, snapshot, start))
})
</script>

<svelte:document onvisibilitychange={() => flush()} />

<MarkdownContent text={displayed} />
