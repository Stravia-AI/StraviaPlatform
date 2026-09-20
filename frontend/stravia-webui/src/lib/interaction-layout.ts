import dagre from '@dagrejs/dagre'

import { interactionNodeWidth as nodeWidth, interactionNodeHeight as nodeHeight } from './interaction-node-geometry'
import type { InteractionSummary } from '$lib/types'

export interface LayoutPosition {
  id: string
  rootId: string
  x: number
  y: number
}

export interface LayoutRoot {
  id: string
  startedAt: number
  interactions: Pick<InteractionSummary, 'id'>[]
}

export interface LayoutRequest {
  requestId: number
  roots: LayoutRoot[]
  edges: { source: string; target: string }[]
}

export interface LayoutResponse {
  requestId: number
  positions: LayoutPosition[]
}

export interface CachedLayout {
  topology: string
  positions: LayoutPosition[]
  width: number
}

const rootGap = 160

export function layoutForest(request: LayoutRequest, layouts: Map<string, CachedLayout>): LayoutPosition[] {
  const positions: LayoutPosition[] = []
  let columnX = 0
  const roots = new Map(request.roots.map((root) => [root.id, root]))
  const nodeRoots = new Map(
    request.roots.flatMap((root) => root.interactions.map((interaction) => [interaction.id, root.id] as const)),
  )
  // 根节点只按观察关联做无向连通分组，用并查集替代 graphlib（其 Graph 默认泛型是 any）。
  const rootParent = new Map(request.roots.map((root) => [root.id, root.id]))
  const findRoot = (id: string): string => {
    let root = id
    let next = rootParent.get(root)
    while (next !== undefined && next !== root) {
      root = next
      next = rootParent.get(root)
    }
    return root
  }
  const rootEdges = new Map<string, LayoutRequest['edges']>()
  for (const edge of request.edges) {
    const source = nodeRoots.get(edge.source)
    const target = nodeRoots.get(edge.target)
    if (!source || !target) continue
    rootParent.set(findRoot(source), findRoot(target))
    const edges = rootEdges.get(source)
    if (edges) edges.push(edge)
    else rootEdges.set(source, [edge])
  }
  const componentMap = new Map<string, string[]>()
  for (const root of request.roots) {
    const key = findRoot(root.id)
    const component = componentMap.get(key)
    if (component) component.push(root.id)
    else componentMap.set(key, [root.id])
  }
  // 观察关联只合并画布布局分组，不改写后端的执行父链或根节点身份。
  // 列顺序按根请求 started_at 新→左；不用 last_active_at，避免运行中的链左右跳动。
  const groups = [...componentMap.values()]
    .map((ids) => ({
      id: [...ids].sort()[0],
      startedAt: Math.max(0, ...ids.map((id) => roots.get(id)?.startedAt ?? 0)),
      interactions: ids.flatMap((id) => roots.get(id)?.interactions ?? []),
      edges: ids.flatMap((id) => rootEdges.get(id) ?? []),
    }))
    .toSorted((left, right) => right.startedAt - left.startedAt || left.id.localeCompare(right.id))
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
        const point = graph.node(interaction.id) as { x: number; y: number }
        return { id: interaction.id, rootId: root.id, x: point.x - nodeWidth / 2, y: point.y - nodeHeight / 2 }
      })
      const minX = Math.min(...laidOut.map((position) => position.x))
      const maxX = Math.max(...laidOut.map((position) => position.x + nodeWidth))
      const relativePositions = laidOut.map((position) => ({ ...position, x: position.x - minX }))
      layout = { topology, positions: relativePositions, width: maxX - minX }
      layouts.set(root.id, layout)
    }
    for (const position of layout.positions) positions.push({ ...position, x: columnX + position.x })
    columnX += layout.width + rootGap
  }

  return positions
}
