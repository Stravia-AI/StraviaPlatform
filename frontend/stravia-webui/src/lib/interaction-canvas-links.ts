import type { InteractionSummary } from '$lib/types'

export type CanvasLinkKind = 'confirmed' | 'inferred' | 'native'

export interface CanvasLink {
  id: string
  source: string
  target: string
  kind: CanvasLinkKind
}

function contextSources(
  interaction: InteractionSummary,
  visible: Set<string>,
): { inferred: string[]; native: string[] } {
  const inferred: string[] = []
  const native: string[] = []
  const seen = new Set<string>()
  for (const event of interaction.context_events ?? []) {
    const nativeEvent = event.kind === 'native_compaction_associated'
    const inferredEvent =
      event.kind === 'retained_tail_associated' &&
      event.payload &&
      typeof event.payload === 'object' &&
      (event.payload as Record<string, unknown>).status === 'inferred'
    if (!nativeEvent && !inferredEvent) continue
    if (!event.payload || typeof event.payload !== 'object') continue
    const source = (event.payload as Record<string, unknown>).source_interaction_id
    if (typeof source !== 'string' || source === interaction.id || !visible.has(source) || seen.has(source)) continue
    seen.add(source)
    if (nativeEvent) native.push(source)
    else inferred.push(source)
  }
  return { inferred, native }
}

function rawParents(interaction: InteractionSummary, visible: Set<string>): string[] {
  const { inferred, native } = contextSources(interaction, visible)
  const parents: string[] = []
  const seen = new Set<string>()
  for (const id of [interaction.parent_interaction_id, ...native, ...inferred]) {
    if (!id || !visible.has(id) || id === interaction.id || seen.has(id)) continue
    seen.add(id)
    parents.push(id)
  }
  return parents
}

function underConfirmed(
  node: InteractionSummary,
  confirmed: string,
  byId: Map<string, InteractionSummary>,
  visible: Set<string>,
): boolean {
  const seen = new Set<string>()
  let current: InteractionSummary | undefined = node
  while (current && !seen.has(current.id)) {
    seen.add(current.id)
    if (current.id === confirmed) return true
    const parentId = rawParents(current, visible)[0]
    current = parentId ? byId.get(parentId) : undefined
  }
  return false
}

export function visualParent(
  interaction: InteractionSummary,
  interactions: InteractionSummary[],
): { id: string; kind: CanvasLinkKind } | null {
  const visible = new Set(interactions.map((item) => item.id))
  const byId = new Map(interactions.map((item) => [item.id, item]))
  const { inferred, native } = contextSources(interaction, visible)
  const confirmed =
    interaction.parent_interaction_id && visible.has(interaction.parent_interaction_id)
      ? interaction.parent_interaction_id
      : null
  if (confirmed && native.includes(confirmed)) {
    return { id: confirmed, kind: 'native' }
  }
  if (confirmed) {
    const intermediates = interactions.filter((other) => {
      if (other.id === interaction.id || other.id === confirmed || other.started_at >= interaction.started_at) {
        return false
      }
      if (other.parent_interaction_id === confirmed) return false
      return underConfirmed(other, confirmed, byId, visible)
    })
    const spineSources = inferred.filter((id) => intermediates.some((item) => item.id === id))
    if (spineSources.length > 0) {
      spineSources.sort((left, right) => {
        const leftNode = byId.get(left)
        const rightNode = byId.get(right)
        return (leftNode?.started_at ?? 0) - (rightNode?.started_at ?? 0) || left.localeCompare(right)
      })
      const head = spineSources[spineSources.length - 1]
      return { id: head, kind: native.includes(head) ? 'native' : 'inferred' }
    }
    const tailRecorded = (interaction.context_events ?? []).some((event) => event.kind === 'retained_tail_associated')
    // 旧记录在有 Generation parent 时跳过了尾部匹配；没有事件时仍按时间接到中间轮。
    if (!tailRecorded && intermediates.length > 0) {
      intermediates.sort((left, right) => left.started_at - right.started_at || left.id.localeCompare(right.id))
      const head = intermediates[intermediates.length - 1]
      return { id: head.id, kind: native.includes(head.id) ? 'native' : 'inferred' }
    }
    return { id: confirmed, kind: 'confirmed' }
  }
  if (native[0]) return { id: native[0], kind: 'native' }
  if (inferred[0]) return { id: inferred[0], kind: 'inferred' }
  return null
}

export function canvasLinks(interactions: InteractionSummary[]): CanvasLink[] {
  const visible = new Set(interactions.map((item) => item.id))
  const links: CanvasLink[] = []
  const seen = new Set<string>()
  for (const interaction of interactions) {
    const parent = visualParent(interaction, interactions)
    if (!parent || !visible.has(parent.id)) continue
    const id = `${parent.kind}-${parent.id}-${interaction.id}`
    if (seen.has(id)) continue
    seen.add(id)
    links.push({ id, source: parent.id, target: interaction.id, kind: parent.kind })
  }
  return links
}
