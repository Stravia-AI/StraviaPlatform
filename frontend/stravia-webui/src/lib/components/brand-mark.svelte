<script lang="ts">
import { onMount } from 'svelte'
import brandMarkup from '../../assets/logos/stravia-logo.svg?raw'

import { cn } from '$lib/utils.js'

interface Props {
  class?: string
  label?: string
  state?: 'static' | 'launch' | 'running' | 'complete'
}

let { class: className, label = 'Stravia 观策行', state: motionState = 'static' }: Props = $props()
let visible = $state(true)

onMount(() => {
  const updateVisibility = () => {
    visible = !document.hidden
  }
  updateVisibility()
  document.addEventListener('visibilitychange', updateVisibility)
  return () => document.removeEventListener('visibilitychange', updateVisibility)
})
</script>

<span
  class={cn('brand-mark inline-block shrink-0 text-foreground', className)}
  data-state={visible ? motionState : 'static'}
  role="img"
  aria-label={label}>
  <!-- The markup is a checked-in, build-time SVG asset, never user or API content. -->
  <!-- eslint-disable-next-line svelte/no-at-html-tags -->
  {@html brandMarkup}
</span>

<style>
.brand-mark :global(svg) {
  display: block;
}
.brand-mark :global([data-unit='lower']) {
  --entry-x: -12px;
  --entry-y: 18px;
  --drift: 3px;
  --delay: 0ms;
}
.brand-mark :global([data-unit='middle']) {
  --entry-x: -16px;
  --entry-y: 16px;
  --drift: 4px;
  --delay: 110ms;
}
.brand-mark :global([data-unit='upper']) {
  --entry-x: -18px;
  --entry-y: 12px;
  --drift: 5px;
  --delay: 220ms;
}
.brand-mark[data-state='launch'] :global(.cadence-motion) {
  animation: cadence-enter 560ms cubic-bezier(0.2, 0, 0, 1) var(--delay) both;
}
.brand-mark[data-state='running'] :global(.cadence-motion) {
  animation: cadence-run 1600ms ease-in-out var(--delay) infinite;
}
.brand-mark[data-state='complete'] :global(.cadence-motion) {
  animation: cadence-settle 440ms cubic-bezier(0.2, 0, 0, 1) both;
}
@keyframes cadence-enter {
  from {
    transform: translate(var(--entry-x), var(--entry-y));
    opacity: 0.28;
  }
  to {
    transform: translate(0, 0);
    opacity: 1;
  }
}
@keyframes cadence-run {
  0%,
  70%,
  100% {
    transform: translate(0, 0);
  }
  35% {
    transform: translate(var(--drift), calc(-1 * var(--drift)));
  }
}
@keyframes cadence-settle {
  from {
    transform: translate(var(--drift), calc(-1 * var(--drift)));
  }
  to {
    transform: translate(0, 0);
  }
}
@media (prefers-reduced-motion: reduce) {
  .brand-mark :global(.cadence-motion) {
    animation: none !important;
    transform: none;
    opacity: 1;
  }
}
</style>
