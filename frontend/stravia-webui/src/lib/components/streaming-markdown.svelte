<script lang="ts">
import { untrack } from 'svelte'
import MarkdownContent from '$lib/components/markdown-content.svelte'

let { text, active = false }: { text: string; active?: boolean } = $props()

const segmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' })
let received = untrack(() => text)
let displayed = $state(received)
let visibleEnd = received.length
let lastVisibleStart = segmenter.segment(received).containing(Math.max(0, visibleEnd - 1))?.index ?? 0
let pending: { start: number; end: number }[] = []
let cursor = 0
let frame: number | null = null
let lastTime = 0
let credit = 0
let rate = 1 / 25
let reducedMotion = false

function cancelFrame() {
  if (frame !== null) cancelAnimationFrame(frame)
  frame = null
  credit = 0
  lastTime = 0
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

function synchronize(next: string, animate: boolean) {
  const append = next.startsWith(received)
  const changed = next !== received
  received = next
  if (!animate || reducedMotion || !append) {
    flush()
    return
  }
  if (!changed) return

  // Re-segment from the last visible grapheme: a new chunk can extend it with
  // a combining mark, variation selector, ZWJ sequence, or surrogate pair.
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

  // Preserve a readable cadence for small deltas; a bulk append catches up
  // within roughly 400ms rather than building a seconds-long replay queue.
  rate = Math.max(rate, pending.length / 400)
  if (frame === null) {
    lastTime = performance.now()
    frame = requestAnimationFrame(reveal)
  }
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
  // Only incoming props drive this effect, never the animation's own state.
  untrack(() => synchronize(next, animate))
})
</script>

<MarkdownContent text={displayed} />
