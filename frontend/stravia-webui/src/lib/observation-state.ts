import type { InteractionDetail, LiveContentBlock, ObservationEvent, RunDetail } from './types/observation'

export function isLiveContentBlock(value: unknown): value is LiveContentBlock {
  if (!value || typeof value !== 'object') return false
  return 'block_id' in value && typeof value.block_id === 'string' &&
    'interaction_id' in value && typeof value.interaction_id === 'string' &&
    'run_id' in value && typeof value.run_id === 'string' &&
    'kind' in value && ['client_visible_content_delta', 'model_thinking_delta'].includes(String(value.kind)) &&
    'text' in value && typeof value.text === 'string' &&
    'revision' in value && Number.isSafeInteger(value.revision) && Number(value.revision) >= 0 &&
    'occurred_at' in value && Number.isSafeInteger(value.occurred_at) &&
    'model_turn_id' in value && (value.model_turn_id === null || typeof value.model_turn_id === 'string') &&
    'attempt_id' in value && (value.attempt_id === null || typeof value.attempt_id === 'string')
}

export function eventBlockId(event: ObservationEvent): string | undefined {
  const payload = event.payload
  return payload && typeof payload === 'object' && 'block_id' in payload && typeof payload.block_id === 'string'
    ? payload.block_id : undefined
}

/** Only the selected history is retained; background live previews have a separate small budget. */
export function retainLiveBlocks(blocks: LiveContentBlock[], selectedId?: string): LiveContentBlock[] {
  let selectedBytes = 0
  let previewBytes = 0
  let previews = 0
  return blocks.toReversed().filter((block) => {
    const bytes = block.text.length * 2
    if (block.interaction_id === selectedId) {
      selectedBytes += bytes
      return selectedBytes <= 8 * 1024 * 1024
    }
    previewBytes += bytes
    return ++previews <= 32 && previewBytes <= 256 * 1024
  }).reverse()
}

export function mergeObservationRuns(current: RunDetail[], incoming: RunDetail[], older = false): RunDetail[] {
  const byId = new Map(incoming.map((run) => [run.id, run]))
  const runs = current.map((run) => {
    const next = byId.get(run.id)
    if (!next) return run
    byId.delete(run.id)
    const sequences = new Set(run.events.map((event) => event.sequence))
    const added = next.events.filter((event) => !sequences.has(event.sequence))
    const events = added.length ? [...run.events, ...added].sort((a, b) => a.sequence - b.sequence) : run.events
    const metadata = older ? run : next
    // Metadata is small; historical event payloads are never compared or copied.
    const unchanged = Object.keys(metadata).every((key) => key === 'events' ||
      JSON.stringify(metadata[key as keyof RunDetail]) === JSON.stringify(run[key as keyof RunDetail]))
    return unchanged && events === run.events ? run : { ...metadata, events }
  })
  if (byId.size) runs.push(...byId.values())
  return runs.sort((a, b) => a.started_at - b.started_at || a.id.localeCompare(b.id))
}

export function withoutCommittedBlocks(blocks: LiveContentBlock[], detail: InteractionDetail): LiveContentBlock[] {
  const committed = new Set(detail.runs.flatMap((run) => run.events.map(eventBlockId)))
  return blocks.filter((block) => !committed.has(block.block_id))
}
