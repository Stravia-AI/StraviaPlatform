import type { InteractionDetail, LiveContentBlock, RunDetail } from './types/observation'

export interface ObservationChatMessage {
  id: string
  role: 'user' | 'assistant'
  text: string
  at: number
  model: string
  live: boolean
  unsaved: boolean
}

const committedText = new WeakMap<RunDetail, string>()
function runText(run: RunDetail): string {
  const cached = committedText.get(run)
  if (cached !== undefined) return cached
  const text = run.events.filter((event) => event.kind === 'client_visible_content_delta' && event.run_id === run.id)
    .toSorted((a, b) => a.sequence - b.sequence).map((event) => {
      const payload = event.payload
      return payload && typeof payload === 'object' && 'text' in payload && typeof payload.text === 'string' ? payload.text : ''
    }).join('')
  committedText.set(run, text)
  return text
}

export function observationConversationMessages(detail: InteractionDetail, blocks: LiveContentBlock[] = [], previous: ObservationChatMessage[] = []): ObservationChatMessage[] {
  const interaction = detail.interaction
  const model = interaction.first_model_display_name?.trim() || interaction.first_route_id
  const messages: ObservationChatMessage[] = [{ id: `user:${interaction.id}`, role: 'user', text: interaction.input_preview ?? '', at: interaction.started_at, model: '', live: false, unsaved: false }]
  const runs = detail.runs.toSorted((a, b) => a.started_at - b.started_at)
  const pendingBlocks = blocks.filter((block) => block.interaction_id === interaction.id)
  const visible = pendingBlocks.filter((block) => block.kind === 'client_visible_content_delta')
  for (const run of runs) {
    const pending = visible.filter((block) => block.run_id === run.id)
    messages.push({ id: `assistant:${run.id}`, role: 'assistant', text: runText(run) + pending.map((block) => block.text).join(''), at: run.started_at,
      model: run.model_display_name?.trim() || run.route_id, live: run.status === 'running', unsaved: pendingBlocks.some((block) => block.run_id === run.id) })
  }
  // A new run can stream before its durable metadata is fetched. Its stable run key survives that fetch.
  for (const runId of new Set(pendingBlocks.filter((block) => !runs.some((run) => run.id === block.run_id)).map((block) => block.run_id))) {
    const pending = visible.filter((block) => block.run_id === runId)
    messages.push({ id: `assistant:${runId}`, role: 'assistant', text: pending.map((block) => block.text).join(''), at: pendingBlocks.find((block) => block.run_id === runId)!.occurred_at, model, live: true, unsaved: true })
  }
  if (messages.length === 1) messages.push({ id: `assistant:${interaction.id}`, role: 'assistant', text: '', at: interaction.started_at, model, live: interaction.status === 'running', unsaved: false })
  // Legacy fallback is only valid for a complete history, never for a truncated event page.
  if (detail.older_events_cursor === null && !messages.some((message) => message.role === 'assistant' && message.text) && interaction.visible_tail) messages[messages.length - 1].text = interaction.visible_tail
  const byId = new Map(previous.map((message) => [message.id, message]))
  return messages.map((message) => {
    const prior = byId.get(message.id)
    return prior && prior.text === message.text && prior.live === message.live && prior.unsaved === message.unsaved && prior.model === message.model && prior.at === message.at ? prior : message
  })
}
