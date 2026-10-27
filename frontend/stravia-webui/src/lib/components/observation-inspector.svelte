<script lang="ts">
import * as m from '#lib/paraglide/messages.js'
import { onDestroy } from 'svelte'
import type { CredentialHitHighlight } from '#lib/credential-hit-highlights.js'
import type { InteractionDetail, LiveContentBlock, FailedRequestDetail } from '#lib/types.js'
import ObservationInspectorContent from '#lib/components/observation-inspector-content.svelte'
import * as Sheet from '#lib/components/ui/sheet/index.js'

interface Props {
  interaction?: InteractionDetail
  failure?: FailedRequestDetail
  error?: string
  onretry?: () => void
  oninteraction?: () => void
  liveBlocks?: LiveContentBlock[]
  liveActive?: boolean
  liveContentEpoch?: number
  liveGap?: boolean
  liveCapacity?: boolean
  olderLoading?: boolean
  onolder?: () => Promise<void>
  credentialHits?: CredentialHitHighlight
  loading?: boolean
  /** Chosen panel width in rem; clamped to the workspace before it is applied. */
  width: number
  /** Measured width of the workspace the panel overlays, in px. 0 until measured. */
  containerWidth: number
  onwidthchange: (width: number) => void
  onclose: () => void
  onbundle: () => void
  onlatest?: () => void
  portalTarget?: HTMLElement
}

let {
  interaction,
  failure,
  error,
  onretry,
  oninteraction,
  liveBlocks = [],
  liveActive = true,
  liveContentEpoch = 0,
  liveGap = false,
  liveCapacity = false,
  olderLoading = false,
  onolder,
  credentialHits,
  loading = false,
  width,
  containerWidth,
  onwidthchange,
  onclose,
  onbundle,
  onlatest,
  portalTarget,
}: Props = $props()
/** Inspector bounds in rem. The canvas keeps CANVAS_MIN_REM; when that leaves less than MIN_REM the panel stacks as a Sheet. */
const MIN_REM = 28
const MAX_REM = 56
const CANVAS_MIN_REM = 18
const KEY_STEP_REM = 1
const previousFocus = typeof document === 'undefined' ? null : document.activeElement
let activeTab = $state('timeline')
let resizing = $state(false)
let panel = $state<HTMLElement>()
let stopResize: (() => void) | undefined

function remPx(): number {
  return (typeof document !== 'undefined' && parseFloat(getComputedStyle(document.documentElement).fontSize)) || 16
}

const maxRem = $derived(containerWidth > 0 ? Math.min(MAX_REM, containerWidth / remPx() - CANVAS_MIN_REM) : MAX_REM)
// Side by side only when both the panel and a usable canvas fit in the workspace, not by viewport.
const stacked = $derived(maxRem < MIN_REM)
const appliedRem = $derived(Math.max(MIN_REM, Math.min(width, maxRem)))

$effect(() => {
  if (stacked) stopResize?.()
  else panel?.focus()
})

onDestroy(() => {
  stopResize?.()
  if (!stacked) restoreFocus()
})

function restoreFocus(): void {
  if (typeof HTMLElement !== 'undefined' && previousFocus instanceof HTMLElement && previousFocus.isConnected) {
    previousFocus.focus()
  }
}

function handleKey(event: KeyboardEvent): void {
  // 浮层可在同一次按键传播中先还原焦点；关闭归属应取按键发生时的目标。
  if (
    event.defaultPrevented ||
    stacked ||
    event.key !== 'Escape' ||
    !(event.target instanceof Node) ||
    !panel?.contains(event.target)
  )
    return
  event.preventDefault()
  onclose()
}

function clampRem(value: number): number {
  return Math.min(maxRem, Math.max(MIN_REM, value))
}

function beginResize(event: PointerEvent): void {
  event.preventDefault()
  stopResize?.()
  resizing = true
  const startX = event.clientX
  const startWidth = appliedRem
  const rem = remPx()
  const move = (next: PointerEvent) => onwidthchange(clampRem(startWidth + (startX - next.clientX) / rem))
  const stop = () => {
    resizing = false
    window.removeEventListener('pointermove', move)
    window.removeEventListener('pointerup', stop)
    window.removeEventListener('pointercancel', stop)
    stopResize = undefined
  }
  stopResize = stop
  window.addEventListener('pointermove', move)
  window.addEventListener('pointerup', stop, { once: true })
  window.addEventListener('pointercancel', stop, { once: true })
}

function resizeWithKeyboard(event: KeyboardEvent): void {
  if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
  event.preventDefault()
  onwidthchange(clampRem(appliedRem + (event.key === 'ArrowRight' ? KEY_STEP_REM : -KEY_STEP_REM)))
}
</script>

<svelte:window onkeydown={handleKey} />

{#snippet content()}
  <ObservationInspectorContent
    {interaction}
    {failure}
    {error}
    {onretry}
    {oninteraction}
    {loading}
    {liveBlocks}
    {liveActive}
    {liveContentEpoch}
    {liveGap}
    {liveCapacity}
    {olderLoading}
    {onolder}
    {credentialHits}
    bind:activeTab
    {onclose}
    {onbundle}
    {onlatest} />
{/snippet}

{#if stacked}
  <Sheet.Root
    open
    onOpenChange={(open) => {
      if (!open) onclose()
    }}>
    <Sheet.Content
      portalProps={{ to: portalTarget }}
      side="right"
      class="w-full! max-w-none! gap-0"
      showCloseButton={false}
      aria-describedby={undefined}
      onCloseAutoFocus={(event: Event) => {
        event.preventDefault()
        if (stacked) restoreFocus()
        else panel?.focus()
      }}>
      <Sheet.Title class="sr-only">{m.observation_details()}</Sheet.Title>
      {@render content()}
    </Sheet.Content>
  </Sheet.Root>
{:else}
  <aside
    bind:this={panel}
    tabindex="-1"
    class={['inspector', resizing && 'resizing']}
    style:--inspector-width={`${appliedRem}rem`}
    aria-label={m.observation_details()}>
    <div
      class="resize-handle"
      aria-label={m.observation_resize_inspector()}
      aria-orientation="horizontal"
      role="slider"
      aria-valuenow={Math.round(appliedRem * 10) / 10}
      aria-valuemin={MIN_REM}
      aria-valuemax={Math.max(MIN_REM, Math.round(maxRem * 10) / 10)}
      tabindex="0"
      onpointerdown={beginResize}
      onkeydown={resizeWithKeyboard}>
    </div>
    {@render content()}
  </aside>
{/if}

<style>
.inspector {
  position: absolute;
  inset-block: 0;
  inset-inline-end: 0;
  z-index: 5;
  display: flex;
  width: var(--inspector-width);
  max-width: 100%;
  flex-direction: column;
  border-inline-start: 1px solid var(--border);
  background: var(--background);
  box-shadow: var(--shadow-lg);
}
.resize-handle {
  position: absolute;
  z-index: 1;
  inset-block: 0;
  inset-inline-start: -0.35rem;
  width: 0.7rem;
  cursor: col-resize;
  background: transparent;
}
.resize-handle:focus-visible {
  outline-offset: -3px;
}
.resizing {
  user-select: none;
}
</style>
