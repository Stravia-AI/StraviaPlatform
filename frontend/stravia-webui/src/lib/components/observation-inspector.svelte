<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onDestroy } from 'svelte'
import { MediaQuery } from 'svelte/reactivity'
import type { InteractionDetail, RejectionDetail } from '$lib/types'
import ObservationInspectorContent from '$lib/components/observation-inspector-content.svelte'
import * as Sheet from '$lib/components/ui/sheet'

interface Props {
  interaction?: InteractionDetail
  rejection?: RejectionDetail
  loading?: boolean
  width: number
  onwidthchange: (width: number) => void
  onclose: () => void
  onbundle: () => void
  onlatest?: () => void
  portalTarget?: HTMLElement
}

let {
  interaction,
  rejection,
  loading = false,
  width,
  onwidthchange,
  onclose,
  onbundle,
  onlatest,
  portalTarget,
}: Props = $props()
const mobile = new MediaQuery('(max-width: 767px)', false)
const previousFocus = typeof document === 'undefined' ? null : document.activeElement
let activeTab = $state('timeline')
let resizing = $state(false)
let panel = $state<HTMLElement>()
let stopResize: (() => void) | undefined

$effect(() => {
  if (mobile.current) stopResize?.()
  else panel?.focus()
})

$effect(() => {
  if (loading || activeTab !== 'debug') return
  const hasDebug = interaction
    ? interaction.runs.some((run) => run.debug_enabled)
    : Boolean(rejection?.rejection.debug_enabled)
  if (!hasDebug && activeTab === 'debug') activeTab = 'timeline'
})

onDestroy(() => {
  stopResize?.()
  if (!mobile.current) restoreFocus()
})

function restoreFocus(): void {
  if (typeof HTMLElement !== 'undefined' && previousFocus instanceof HTMLElement && previousFocus.isConnected) {
    previousFocus.focus()
  }
}

function handleKey(event: KeyboardEvent): void {
  if (mobile.current || !panel?.contains(document.activeElement) || event.key !== 'Escape') return
  event.preventDefault()
  onclose()
}

function beginResize(event: PointerEvent): void {
  event.preventDefault()
  stopResize?.()
  resizing = true
  const startX = event.clientX
  const startWidth = width
  const move = (next: PointerEvent) =>
    onwidthchange(Math.min(55, Math.max(40, startWidth + ((startX - next.clientX) / innerWidth) * 100)))
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
  onwidthchange(Math.min(55, Math.max(40, width + (event.key === 'ArrowRight' ? 1 : -1))))
}
</script>

<svelte:window onkeydown={handleKey} />

{#snippet content()}
  <ObservationInspectorContent {interaction} {rejection} {loading} bind:activeTab {onclose} {onbundle} {onlatest} />
{/snippet}

{#if mobile.current}
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
      onCloseAutoFocus={(event) => {
        event.preventDefault()
        if (mobile.current) restoreFocus()
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
    style:--inspector-width={`${width}vw`}
    aria-label={m.observation_details()}>
    <div
      class="resize-handle"
      aria-label={m.observation_resize_inspector()}
      aria-orientation="horizontal"
      role="slider"
      aria-valuenow={width}
      aria-valuemin={40}
      aria-valuemax={55}
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
  min-width: 30rem;
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
