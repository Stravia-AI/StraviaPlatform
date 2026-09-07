/// <reference lib="webworker" />

import dagre from '@dagrejs/dagre'

import type { InteractionSummary } from '$lib/types'

export interface LayoutPosition {
  id: string
  rootId: string
  x: number
  y: number
}

export interface LayoutRequest {
  requestId: number
  roots: { id: string; interactions: Pick<InteractionSummary, 'id' | 'parent_interaction_id'>[] }[]
}

export interface LayoutResponse {
  requestId: number
  positions: LayoutPosition[]
}

const nodeWidth = 288
const nodeHeight = 190
const rootGap = 160
const layouts = new Map<string, { topology: string; positions: LayoutPosition[]; width: number }>()

self.onmessage = (message: MessageEvent<LayoutRequest>) => {
  const positions: LayoutPosition[] = []
  let columnX = 0
  const retainedIds = new Set(message.data.roots.map((root) => root.id))
  for (const id of layouts.keys()) if (!retainedIds.has(id)) layouts.delete(id)

  for (const root of message.data.roots) {
    const topology = JSON.stringify(root.interactions)
    let layout = layouts.get(root.id)
    if (!layout || layout.topology !== topology) {
      const graph = new dagre.graphlib.Graph()
      graph.setGraph({ rankdir: 'TB', ranksep: 76, nodesep: 40, marginx: 0, marginy: 0 })
      graph.setDefaultEdgeLabel(() => ({}))
      for (const interaction of root.interactions)
        graph.setNode(interaction.id, { width: nodeWidth, height: nodeHeight })
      for (const interaction of root.interactions) {
        if (interaction.parent_interaction_id && graph.hasNode(interaction.parent_interaction_id)) {
          graph.setEdge(interaction.parent_interaction_id, interaction.id)
        }
      }
      dagre.layout(graph)
      const laidOut = root.interactions.map((interaction) => {
        const point = graph.node(interaction.id)
        return { id: interaction.id, rootId: root.id, x: point.x - nodeWidth / 2, y: point.y - nodeHeight / 2 }
      })
      const minX = Math.min(...laidOut.map((position) => position.x))
      const maxX = Math.max(...laidOut.map((position) => position.x + nodeWidth))
      const relativePositions = laidOut.map((position) => ({ ...position, x: position.x - minX }))
      layout = { topology, positions: relativePositions, width: Math.max(layout?.width ?? 0, maxX - minX) }
      layouts.set(root.id, layout)
    }
    for (const position of layout.positions) positions.push({ ...position, x: columnX + position.x })
    columnX += layout.width + rootGap
  }

  const response: LayoutResponse = { requestId: message.data.requestId, positions }
  self.postMessage(response)
}
