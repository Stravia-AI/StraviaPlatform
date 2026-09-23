import { payloadRecord } from '$lib/observation-payload'
import type { InteractionSummary } from '$lib/types'

export type CanvasLinkKind = 'confirmed' | 'inferred' | 'native'

export interface CanvasLink {
  id: string
  source: string
  target: string
  kind: CanvasLinkKind
}

interface ContextSources {
  inferred: string[]
  native: string[]
}

function contextSources(interaction: InteractionSummary, visible: ReadonlySet<string>): ContextSources {
  const inferred: string[] = []
  const native: string[] = []
  const seen = new Set<string>()
  for (const event of interaction.context_events ?? []) {
    const nativeEvent = event.kind === 'native_compaction_associated'
    const inferredEvent =
      event.kind === 'retained_tail_associated' && payloadRecord(event.payload).status === 'inferred'
    if (!nativeEvent && !inferredEvent) continue
    const source = payloadRecord(event.payload).source_interaction_id
    if (typeof source !== 'string' || source === interaction.id || !visible.has(source) || seen.has(source)) continue
    seen.add(source)
    if (nativeEvent) native.push(source)
    else inferred.push(source)
  }
  return { inferred, native }
}

/**
 * 同一快照内复用父边索引；输入变化后必须重建。
 * first-parent 无环部分用 DFS 区间判定祖先，带环分量保留有循环防护的回溯。
 */
export class CanvasLinkIndex {
  readonly interactions: readonly InteractionSummary[]
  readonly visible: ReadonlySet<string>
  readonly byId: ReadonlyMap<string, InteractionSummary>
  #byStart: InteractionSummary[] | undefined
  readonly #sources = new Map<string, ContextSources>()
  readonly #firstParents = new Map<string, string | undefined>()
  #ancestry: { tin: Map<string, number>; tout: Map<string, number> } | undefined
  readonly #fallbackAncestry = new Map<string, Map<string, boolean>>()

  constructor(interactions: InteractionSummary[]) {
    this.interactions = interactions
    this.byId = new Map(interactions.map((item) => [item.id, item]))
    this.visible = new Set(this.byId.keys())
  }

  #sourcesOf(interaction: InteractionSummary): ContextSources {
    let cached = this.#sources.get(interaction.id)
    if (!cached) {
      cached = contextSources(interaction, this.visible)
      this.#sources.set(interaction.id, cached)
    }
    return cached
  }

  #firstParentOf(interaction: InteractionSummary): string | undefined {
    if (this.#firstParents.has(interaction.id)) return this.#firstParents.get(interaction.id)
    const confirmed = interaction.parent_interaction_id
    const { native, inferred } = this.#sourcesOf(interaction)
    const parent =
      confirmed && confirmed !== interaction.id && this.visible.has(confirmed) ? confirmed : (native[0] ?? inferred[0])
    this.#firstParents.set(interaction.id, parent)
    return parent
  }

  /**
   * first-parent 森林的 DFS 区间：tin[a] <= tin[x] 且 tout[x] <= tout[a]
   * 当且仅当从 x 沿 first-parent 链能到 a。环上节点无区间，返回 undefined。
   */
  #buildAncestry(): { tin: Map<string, number>; tout: Map<string, number> } {
    const children = new Map<string, string[]>()
    const roots: string[] = []
    for (const node of this.interactions) {
      const parentId = this.#firstParentOf(node)
      if (parentId) {
        const siblings = children.get(parentId)
        if (siblings) siblings.push(node.id)
        else children.set(parentId, [node.id])
      } else {
        roots.push(node.id)
      }
    }
    const tin = new Map<string, number>()
    const tout = new Map<string, number>()
    let clock = 0
    // 迭代 DFS：栈元素为 [nodeId, nextChildIndex]
    for (const rootId of roots) {
      if (tin.has(rootId)) continue
      const stack: Array<[string, number]> = [[rootId, 0]]
      tin.set(rootId, clock++)
      while (stack.length > 0) {
        const frame = stack[stack.length - 1]
        const kids = children.get(frame[0])
        if (kids && frame[1] < kids.length) {
          const kid = kids[frame[1]]
          frame[1] += 1
          if (tin.has(kid)) continue
          tin.set(kid, clock++)
          stack.push([kid, 0])
        } else {
          tout.set(frame[0], clock++)
          stack.pop()
        }
      }
    }
    this.#ancestry = { tin, tout }
    return this.#ancestry
  }

  #underConfirmed(node: InteractionSummary, confirmed: string): boolean {
    const ancestry = this.#ancestry ?? this.#buildAncestry()
    const tin = ancestry.tin.get(node.id)
    if (tin !== undefined) {
      // tin 存在则同次 DFS 必然已写 tout；非空断言表达这个不变量。
      const confirmedTin = ancestry.tin.get(confirmed)
      if (confirmedTin === undefined) return false
      return confirmedTin <= tin && ancestry.tout.get(node.id)! <= ancestry.tout.get(confirmed)!
    }
    // first-parent 链带环：DFS 不可达，回退逐节点回溯（带循环防护与共享 memo）。
    let memo = this.#fallbackAncestry.get(confirmed)
    if (!memo) {
      memo = new Map()
      this.#fallbackAncestry.set(confirmed, memo)
    }
    const seen = new Set<string>()
    let current: InteractionSummary | undefined = node
    let found = false
    while (current && !seen.has(current.id)) {
      const known = memo.get(current.id)
      if (known !== undefined) {
        found = known
        break
      }
      seen.add(current.id)
      if (current.id === confirmed) {
        found = true
        break
      }
      const parentId = this.#firstParentOf(current)
      current = parentId ? this.byId.get(parentId) : undefined
    }
    for (const id of seen) memo.set(id, found)
    return found
  }

  /**
   * confirmed 之下、早于 interaction 的非直接子节点，按 (started_at, id) 升序。
   * 只有 inferred 脊线匹配或旧记录时间 fallback 真正需要时才计算。
   */
  #intermediates(interaction: InteractionSummary, confirmed: string): InteractionSummary[] {
    const intermediates: InteractionSummary[] = []
    const byStart = (this.#byStart ??= [...this.interactions].sort(
      (left, right) => left.started_at - right.started_at || left.id.localeCompare(right.id),
    ))
    for (const other of byStart) {
      if (other.started_at >= interaction.started_at) break
      if (other.id === interaction.id || other.id === confirmed || other.parent_interaction_id === confirmed) continue
      if (this.#underConfirmed(other, confirmed)) intermediates.push(other)
    }
    return intermediates
  }

  visualParent(interaction: InteractionSummary): { id: string; kind: CanvasLinkKind } | null {
    const { inferred, native } = this.#sourcesOf(interaction)
    const confirmed =
      interaction.parent_interaction_id && this.visible.has(interaction.parent_interaction_id)
        ? interaction.parent_interaction_id
        : null
    if (confirmed && native.includes(confirmed)) {
      return { id: confirmed, kind: 'native' }
    }
    if (confirmed) {
      const tailRecorded = (interaction.context_events ?? []).some((event) => event.kind === 'retained_tail_associated')
      // 已记录匹配结果但没有推断来源时，不能通过时间顺序另猜父边。
      if (tailRecorded && inferred.length === 0) {
        return { id: confirmed, kind: 'confirmed' }
      }
      if (inferred.length > 0) {
        const spineSources = inferred.filter((id) => {
          const other = this.byId.get(id)
          return (
            other !== undefined &&
            other.id !== interaction.id &&
            other.id !== confirmed &&
            other.started_at < interaction.started_at &&
            other.parent_interaction_id !== confirmed &&
            this.#underConfirmed(other, confirmed)
          )
        })
        if (spineSources.length > 0) {
          spineSources.sort((left, right) => {
            const leftNode = this.byId.get(left)
            const rightNode = this.byId.get(right)
            return (leftNode?.started_at ?? 0) - (rightNode?.started_at ?? 0) || left.localeCompare(right)
          })
          const head = spineSources[spineSources.length - 1]
          return { id: head, kind: native.includes(head) ? 'native' : 'inferred' }
        }
      }
      // 旧记录在有 Generation parent 时跳过了尾部匹配；没有事件时仍按时间接到中间轮。
      if (!tailRecorded) {
        const ids = this.#intermediates(interaction, confirmed)
        if (ids.length > 0) {
          const head = ids[ids.length - 1]
          return { id: head.id, kind: native.includes(head.id) ? 'native' : 'inferred' }
        }
      }
      if (inferred.includes(confirmed)) {
        const generationRoot = interaction.generation_root_id
        // 同一 Generation Chain 的尾部匹配是补充诊断；只有跨根来源才画推断边。
        return {
          id: confirmed,
          kind:
            generationRoot !== null && generationRoot === this.byId.get(confirmed)?.generation_root_id
              ? 'confirmed'
              : 'inferred',
        }
      }
      return { id: confirmed, kind: 'confirmed' }
    }
    if (native[0]) return { id: native[0], kind: 'native' }
    if (inferred[0]) return { id: inferred[0], kind: 'inferred' }
    return null
  }
}

export function visualParent(
  interaction: InteractionSummary,
  interactions: InteractionSummary[],
): { id: string; kind: CanvasLinkKind } | null {
  return new CanvasLinkIndex(interactions).visualParent(interaction)
}

export function canvasLinks(interactions: InteractionSummary[]): CanvasLink[] {
  const index = new CanvasLinkIndex(interactions)
  const links: CanvasLink[] = []
  const seen = new Set<string>()
  for (const interaction of interactions) {
    const parent = index.visualParent(interaction)
    if (!parent || !index.visible.has(parent.id)) continue
    const id = `${parent.kind}-${parent.id}-${interaction.id}`
    if (seen.has(id)) continue
    seen.add(id)
    links.push({ id, source: parent.id, target: interaction.id, kind: parent.kind })
  }
  return links
}
