<script lang="ts">
import * as m from '$lib/paraglide/messages.js'
import { onMount, tick } from 'svelte'
import ChevronDownIcon from '@lucide/svelte/icons/chevron-down'
import CrosshairIcon from '@lucide/svelte/icons/crosshair'
import LocateFixedIcon from '@lucide/svelte/icons/locate-fixed'
import MapIcon from '@lucide/svelte/icons/map'
import MinusIcon from '@lucide/svelte/icons/minus'
import PlusIcon from '@lucide/svelte/icons/plus'
import {
  Background,
  BackgroundVariant,
  MiniMap,
  Panel,
  SvelteFlow,
  useSvelteFlow,
  type Edge,
  type Node,
} from '@xyflow/svelte'
import '@xyflow/svelte/dist/style.css'

import { observationStatusLabel } from '$lib/observation-labels'
import type { ForestRoot, InteractionNodeData, InteractionSummary } from '$lib/types'
import InteractionNode from '$lib/components/interaction-node.svelte'
import { Button } from '$lib/components/ui/button'

type FlowInteractionNode = Node<InteractionNodeData, 'interaction'>

interface Props {
  roots: ForestRoot[]
  selectedId?: string
  selectedPath: Set<string>
  loadingMore: boolean
  nextCursor?: string | null
  rootTotal: number
  latestId?: string
  followPaused: boolean
  newActivityAvailable: boolean
  fitProgress?: number
  onselect: (interaction: InteractionSummary) => void
  onloadmore: () => void
  onfitall: () => void
  onmanualmove: () => void
  onfollow: () => void
}

let {
  roots,
  selectedId,
  selectedPath,
  loadingMore,
  nextCursor,
  rootTotal,
  latestId,
  followPaused,
  newActivityAvailable,
  fitProgress,
  onselect,
  onloadmore,
  onfitall,
  onmanualmove,
  onfollow,
}: Props = $props()

const { fitView, setCenter, zoomIn, zoomOut, getViewport } = useSvelteFlow<FlowInteractionNode, Edge>()
const nodeTypes = { interaction: InteractionNode }
let positions = $state.raw(new Map<string, { x: number; y: number }>())
let canvasElement = $state<HTMLDivElement>()
let minimapOpen = $state(false)
let requestId = 0
let requestedTopology = ''
let initialized = false
let internalMove = false
let worker: Worker | undefined
let pendingLayout = Promise.resolve()
let resolveLayout: (() => void) | undefined

let nodes = $derived.by<FlowInteractionNode[]>(() =>
  roots.flatMap((root) =>
    root.interactions.map((interaction) => ({
      id: interaction.id,
      type: 'interaction' as const,
      position: positions.get(interaction.id) ?? { x: 0, y: 0 },
      data: {
        interaction,
        onSelectedPath: selectedPath.has(interaction.id),
        subdued:
          (selectedId != null && !selectedPath.has(interaction.id)) ||
          (!interaction.matched && roots.some((item) => item.interactions.some((node) => node.matched))),
      },
      selected: interaction.id === selectedId,
      draggable: false,
      connectable: false,
      deletable: false,
      focusable: true,
      ariaRole: 'button' as const,
      ariaLabel: `${interaction.first_model_display_name || interaction.first_route_id}, ${observationStatusLabel(interaction.status)}`,
    })),
  ),
)
let edges = $derived.by<Edge[]>(() =>
  roots.flatMap((root) =>
    root.interactions.flatMap((interaction) =>
      interaction.parent_interaction_id &&
      root.interactions.some((candidate) => candidate.id === interaction.parent_interaction_id)
        ? [
            {
              id: `${interaction.parent_interaction_id}-${interaction.id}`,
              source: interaction.parent_interaction_id,
              target: interaction.id,
              type: 'smoothstep',
              animated: false,
              selectable: false,
              focusable: false,
              style: `stroke: ${selectedPath.has(interaction.parent_interaction_id) && selectedPath.has(interaction.id) ? 'var(--primary)' : 'var(--border)'}; stroke-width: ${selectedPath.has(interaction.parent_interaction_id) && selectedPath.has(interaction.id) ? 2 : 1.25}`,
            },
          ]
        : [],
    ),
  ),
)

async function waitForRenderedLayout(): Promise<void> {
  await tick()
  await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))
}

function cameraDuration(duration: number): number {
  return matchMedia('(prefers-reduced-motion: reduce)').matches ? 0 : duration
}

async function acceptLayout(
  message: MessageEvent<import('$lib/interaction-layout.worker').LayoutResponse>,
): Promise<void> {
  if (message.data.requestId !== requestId) return
  positions = new Map(message.data.positions.map((position) => [position.id, position]))
  await waitForRenderedLayout()
  resolveLayout?.()
  resolveLayout = undefined
  if (!initialized && nodes.length > 0) {
    initialized = true
    if (latestId && !followPaused) await focusNode(latestId)
    else await fitLoaded()
  }
}

function requestLayout(): void {
  if (!worker) return
  const layoutRoots = roots.map((root) => ({
    id: root.id,
    interactions: root.interactions.map(({ id, parent_interaction_id }) => ({ id, parent_interaction_id })),
  }))
  const topology = JSON.stringify(layoutRoots)
  if (topology === requestedTopology) return
  requestedTopology = topology
  if (!resolveLayout) pendingLayout = new Promise<void>((resolve) => (resolveLayout = resolve))
  requestId += 1
  worker.postMessage({ requestId, roots: layoutRoots })
}

onMount(() => {
  worker = new Worker(new URL('../interaction-layout.worker.ts', import.meta.url), { type: 'module' })
  worker.onmessage = acceptLayout
  requestLayout()
  return () => {
    worker?.terminate()
    resolveLayout?.()
  }
})

$effect(() => {
  if (!worker) return
  requestLayout()
})

async function fitLoaded(): Promise<void> {
  await pendingLayout
  await waitForRenderedLayout()
  internalMove = true
  try {
    await fitView({ padding: 0.16, maxZoom: 1, duration: cameraDuration(240) })
  } finally {
    internalMove = false
  }
}

async function focusNode(id: string): Promise<void> {
  await pendingLayout
  await waitForRenderedLayout()
  const target = positions.get(id)
  const root = roots.find((candidate) => candidate.interactions.some((interaction) => interaction.id === id))
  if (!target || !root || !canvasElement) return

  const rootPositions = root.interactions.flatMap((interaction) => {
    const position = positions.get(interaction.id)
    return position ? [position] : []
  })
  const centerX = target.x + 144
  const centerY = target.y + 95
  const minX = Math.min(...rootPositions.map((position) => position.x))
  const maxX = Math.max(...rootPositions.map((position) => position.x + 288))
  const minY = Math.min(...rootPositions.map((position) => position.y))
  const maxY = Math.max(...rootPositions.map((position) => position.y + 190))
  const horizontalReach = Math.max(centerX - minX, maxX - centerX)
  const verticalReach = Math.max(centerY - minY, maxY - centerY)
  const fittingZoom = Math.min(
    1,
    horizontalReach ? (canvasElement.clientWidth * 0.42) / horizontalReach : 1,
    verticalReach ? (canvasElement.clientHeight * 0.42) / verticalReach : 1,
  )
  const zoom = Math.max(0.18, Math.min(getViewport().zoom, fittingZoom))

  internalMove = true
  try {
    await setCenter(centerX, centerY, { zoom, duration: cameraDuration(260) })
  } finally {
    internalMove = false
  }
}

function handleMoveEnd(): void {
  if (!internalMove) onmanualmove()
  if (!nextCursor || loadingMore || nodes.length === 0) return
  const viewport = getViewport()
  const rightmost = Math.max(...nodes.map((node) => node.position.x + 288))
  const visibleRight = (-viewport.x + (canvasElement?.clientWidth ?? 0)) / viewport.zoom
  if (visibleRight > rightmost - 220) onloadmore()
}

function chooseNode(node: FlowInteractionNode): void {
  const interaction = node.data.interaction
  if (interaction.id !== latestId) onmanualmove()
  onselect(interaction)
}

function resumeFollow(): void {
  onfollow()
  if (latestId) void focusNode(latestId)
}

export async function fitAfterAllLoaded(): Promise<void> {
  await fitLoaded()
}
export async function focusLatest(): Promise<void> {
  if (latestId) await focusNode(latestId)
}
</script>

<div bind:this={canvasElement} class="observation-canvas" aria-label={m.observation_interaction_canvas()}>
  <SvelteFlow
    bind:nodes
    bind:edges
    {nodeTypes}
    nodesDraggable={false}
    nodesConnectable={false}
    elementsSelectable
    nodesFocusable
    edgesFocusable={false}
    deleteKey={null}
    selectionKey={null}
    multiSelectionKey={null}
    panOnDrag
    panOnScroll={false}
    zoomOnScroll
    zoomOnPinch
    zoomOnDoubleClick={false}
    preventScrolling
    onlyRenderVisibleElements
    minZoom={0.18}
    maxZoom={1.6}
    ariaLabelConfig={{
      'node.a11yDescription.default': m.observation_node_keyboard_help(),
      'node.a11yDescription.keyboardDisabled': m.observation_node_keyboard_help(),
    }}
    defaultEdgeOptions={{ type: 'smoothstep' }}
    onnodeclick={({ node }) => chooseNode(node)}
    onselectionchange={({ nodes: selected }) => {
      const node = selected[0]
      if (node && node.id !== selectedId) chooseNode(node)
    }}
    onmoveend={handleMoveEnd}
    onpaneclick={() => onmanualmove()}>
    <Background variant={BackgroundVariant.Dots} gap={24} size={1} patternColor="var(--border)" />
    {#if minimapOpen}
      <MiniMap
        pannable
        zoomable
        ariaLabel={m.observation_minimap()}
        nodeColor="var(--muted-foreground)"
        maskColor="color-mix(in oklab, var(--background) 70%, transparent)" />
    {/if}
    <Panel position="top-left" class="canvas-progress">
      <span>{m.observation_roots_loaded({ loaded: roots.length, total: rootTotal })}</span>
      {#if loadingMore}<span>{m.observation_loading_more()}</span>{/if}
      {#if fitProgress != null}<progress value={fitProgress} max="100" aria-label={m.observation_loading_all_roots()}
        ></progress
        >{/if}
    </Panel>
    <Panel position="bottom-left" class="canvas-controls">
      <Button
        variant="secondary"
        size="icon"
        aria-label={m.observation_zoom_in()}
        onclick={() => {
          onmanualmove()
          void zoomIn({ duration: cameraDuration(120) })
        }}><PlusIcon /></Button>
      <Button
        variant="secondary"
        size="icon"
        aria-label={m.observation_zoom_out()}
        onclick={() => {
          onmanualmove()
          void zoomOut({ duration: cameraDuration(120) })
        }}><MinusIcon /></Button>
      <Button
        variant="secondary"
        size="icon"
        aria-label={m.observation_fit_all()}
        onclick={() => {
          onmanualmove()
          onfitall()
        }}><CrosshairIcon /></Button>
      <Button
        variant="secondary"
        size="icon"
        aria-label={m.observation_return_running()}
        disabled={!latestId}
        onclick={resumeFollow}><LocateFixedIcon /></Button>
      <Button
        variant="secondary"
        size="icon"
        aria-label={minimapOpen ? m.observation_hide_minimap() : m.observation_show_minimap()}
        onclick={() => (minimapOpen = !minimapOpen)}
        >{#if minimapOpen}<ChevronDownIcon />{:else}<MapIcon />{/if}</Button>
    </Panel>
    {#if followPaused && newActivityAvailable}
      <Panel position="top-center"
        ><Button onclick={resumeFollow}
          ><span class="activity-mark" aria-hidden="true"></span>{m.observation_new_activity_follow()}</Button
        ></Panel>
    {/if}
  </SvelteFlow>
</div>

<style>
.observation-canvas {
  position: absolute;
  inset: 0;
  overflow: hidden;
  contain: layout paint;
  background: var(--background);
}
.observation-canvas :global(.svelte-flow__node) {
  border-radius: var(--radius);
}
.observation-canvas :global(.svelte-flow__node:focus-visible) {
  outline: 2px solid var(--ring);
  outline-offset: 3px;
}
.observation-canvas :global(.canvas-controls) {
  display: flex;
  gap: 0.35rem;
}
.observation-canvas :global(.canvas-progress) {
  display: flex;
  min-width: 11rem;
  align-items: center;
  gap: 0.65rem;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  background: color-mix(in oklab, var(--background) 92%, transparent);
  padding: 0.45rem 0.65rem;
  color: var(--muted-foreground);
  font-family: var(--font-technical);
  font-size: 0.7rem;
  backdrop-filter: blur(8px);
}
.observation-canvas :global(.canvas-progress progress) {
  width: 5rem;
  height: 0.35rem;
  accent-color: var(--primary);
}
.activity-mark {
  width: 0.45rem;
  height: 0.45rem;
  border-radius: 999px;
  background: var(--success);
}
@media (prefers-reduced-motion: reduce) {
  .activity-mark {
    animation: none;
  }
}
</style>
