/// <reference lib="webworker" />

import dagre from '@dagrejs/dagre'

import { interactionNodeWidth as nodeWidth, interactionNodeHeight as nodeHeight } from './interaction-node-geometry'
import type { InteractionSummary } from '$lib/types'

export interface LayoutPosition {
  id: string
  rootId: string
  x: number
  y: number
}

export interface LayoutRequest {
  requestId: number
  roots: { id: string; interactions: Pick<InteractionSummary, 'id'>[] }[]
  edges: { source: string; target: string }[]
}

export interface LayoutResponse {
  requestId: number
  positions: LayoutPosition[]
}

const rootGap = 160
const layouts = new Map<string, { topology: string; positions: LayoutPosition[]; width: number }>()

self.onmessage = (message: MessageEvent<LayoutRequest>) => {
  const positions: LayoutPosition[] = []
  let columnX = 0
  const roots = new Map(message.data.roots.map((root) => [root.id, root]))
  const nodeRoots = new Map(
    message.data.roots.flatMap((root) => root.interactions.map((interaction) => [interaction.id, root.id] as const)),
  )
  const rootGraph = new dagre.graphlib.Graph({ directed: false })
  const rootEdges = new Map<string, LayoutRequest['edges']>()
  for (const root of message.data.roots) rootGraph.setNode(root.id)
  for (const edge of message.data.edges) {
    const source = nodeRoots.get(edge.source)
    const target = nodeRoots.get(edge.target)
    if (!source || !target) continue
    rootGraph.setEdge(source, target)
    const edges = rootEdges.get(source)
    if (edges) edges.push(edge)
    else rootEdges.set(source, [edge])
  }
  // 观察关联只合并画布布局分组，不改写后端的执行父链或根节点身份。
  const groups = dagre.graphlib.alg
    .components(rootGraph)
    .map((ids) => ({
      id: [...ids].sort()[0],
      interactions: ids.flatMap((id) => roots.get(id)?.interactions ?? []),
      edges: ids.flatMap((id) => rootEdges.get(id) ?? []),
    }))
  const retainedIds = new Set(groups.map((group) => group.id))
  for (const id of layouts.keys()) if (!retainedIds.has(id)) layouts.delete(id)

  for (const root of groups) {
    const edges = root.edges
    const topology = JSON.stringify({ interactions: root.interactions, edges })
    let layout = layouts.get(root.id)
    if (!layout || layout.topology !== topology) {
      const graph = new dagre.graphlib.Graph()
      graph.setGraph({ rankdir: 'TB', ranksep: 76, nodesep: 40, marginx: 0, marginy: 0 })
      graph.setDefaultEdgeLabel(() => ({}))
      for (const interaction of root.interactions)
        graph.setNode(interaction.id, { width: nodeWidth, height: nodeHeight })
      for (const edge of edges) graph.setEdge(edge.source, edge.target)
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
