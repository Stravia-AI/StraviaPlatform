import { payloadRecord, payloadString } from './observation-payload'
import type { InteractionDetail, LiveContentBlock, ObservationOutputPreview, RunDetail } from './types/observation'

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
  // ADR-0062：client_visible_content 每个 Canonical Item 一行，text 已是条目完整正文。
  const text = run.events
    .filter((event) => event.kind === 'client_visible_content' && event.run_id === run.id)
    .toSorted((a, b) => a.sequence - b.sequence)
    .map((event) => payloadString(payloadRecord(event.payload).text) ?? '')
    .join('')
  committedText.set(run, text)
  return text
}

function runInputText(run: RunDetail): string | undefined {
  const payload = run.events.find(
    (event) => event.kind === 'input_preview_recorded' && event.run_id === run.id,
  )?.payload
  return payloadString(payloadRecord(payload).text)
}

const previewSegmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' })

function previewSuffix(text: string, limit: number): string {
  let start = text.length
  for (let count = 0; count < limit && start > 0; count += 1) {
    start -= 1
    const unit = text.charCodeAt(start)
    if (unit >= 0xdc00 && unit <= 0xdfff && start > 0) start -= 1
  }
  if (start === 0) return text
  const boundary = previewSegmenter.segment(text).containing(start)
  if (boundary && boundary.index < start) start = boundary.index + boundary.segment.length
  return text.slice(start)
}

/** 画布只取得选中输出的有界尾部，不复制累计详情全文，也不包含 Thinking。 */
export function observationOutputPreview(
  detail: InteractionDetail,
  blocks: LiveContentBlock[],
): ObservationOutputPreview {
  const runs = detail.runs.toSorted((a, b) => a.started_at - b.started_at)
  const committed = new Set(
    runs.flatMap((run) =>
      run.events
        .filter((event) => event.kind === 'client_visible_content')
        .map((event) => payloadString(payloadRecord(event.payload).block_id)),
    ),
  )
  const visible = blocks.filter(
    (block) =>
      block.interaction_id === detail.interaction.id &&
      block.kind === 'client_visible_content_delta' &&
      !committed.has(block.block_id),
  )
  // 先裁剪各片段，再连接有界尾部，避免复制长正文后才裁剪。
  let tail = ''
  let length = 0
  const append = (text: string) => {
    length += text.length
    tail = previewSuffix(tail + previewSuffix(text, 4096), 4096)
  }
  for (const run of runs) {
    const text = runText(run)
    if (length && text) append('\n\n')
    if (text) append(text)
    for (const block of visible.filter((block) => block.run_id === run.id)) {
      append(block.text)
    }
  }
  for (const block of visible.filter((block) => !runs.some((run) => run.id === block.run_id))) {
    append(block.text)
  }
  return length ? { text: tail, start: length - tail.length } : { text: detail.interaction.visible_tail, start: 0 }
}

export function observationConversationMessages(
  detail: InteractionDetail,
  blocks: LiveContentBlock[] = [],
  previous: ObservationChatMessage[] = [],
): ObservationChatMessage[] {
  const interaction = detail.interaction
  const model = interaction.first_model_display_name?.trim() || interaction.first_route_id
  const runs = detail.runs.toSorted((a, b) => a.started_at - b.started_at)
  const firstInput = runs[0] && runInputText(runs[0])
  const messages: ObservationChatMessage[] = [
    {
      id: `user:${interaction.id}`,
      role: 'user',
      text: interaction.input_preview ?? firstInput ?? '',
      at: interaction.started_at,
      model: '',
      live: false,
      unsaved: false,
    },
  ]
  const pendingBlocks = blocks.filter((block) => block.interaction_id === interaction.id)
  const visible = pendingBlocks.filter((block) => block.kind === 'client_visible_content_delta')
  for (const [index, run] of runs.entries()) {
    const input = index === 0 ? firstInput : runInputText(run)
    if (index > 0 && input !== undefined)
      messages.push({
        id: `user:${run.id}`,
        role: 'user',
        text: input,
        at: run.started_at,
        model: '',
        live: false,
        unsaved: false,
      })
    const committed = new Set(
      run.events
        .filter((event) => event.kind === 'client_visible_content' || event.kind === 'model_thinking')
        .map((event) => payloadString(payloadRecord(event.payload).block_id)),
    )
    const pending = visible.filter((block) => block.run_id === run.id && !committed.has(block.block_id))
    messages.push({
      id: `assistant:${run.id}`,
      role: 'assistant',
      text: runText(run) + pending.map((block) => block.text).join(''),
      at: run.started_at,
      model: run.model_display_name?.trim() || run.route_id,
      live: run.status === 'running',
      unsaved: pendingBlocks.some((block) => block.run_id === run.id && !committed.has(block.block_id)),
    })
  }
  // A new run can stream before its durable metadata is fetched. Its stable run key survives that fetch.
  for (const runId of new Set(
    pendingBlocks.filter((block) => !runs.some((run) => run.id === block.run_id)).map((block) => block.run_id),
  )) {
    const pending = visible.filter((block) => block.run_id === runId)
    messages.push({
      id: `assistant:${runId}`,
      role: 'assistant',
      text: pending.map((block) => block.text).join(''),
      at: pendingBlocks.find((block) => block.run_id === runId)!.occurred_at,
      model,
      live: true,
      unsaved: true,
    })
  }
  if (messages.length === 1)
    messages.push({
      id: `assistant:${interaction.id}`,
      role: 'assistant',
      text: '',
      at: interaction.started_at,
      model,
      live: interaction.status === 'running',
      unsaved: false,
    })
  const byId = new Map(previous.map((message) => [message.id, message]))
  return messages.map((message) => {
    const prior = byId.get(message.id)
    return prior &&
      prior.text === message.text &&
      prior.live === message.live &&
      prior.unsaved === message.unsaved &&
      prior.model === message.model &&
      prior.at === message.at
      ? prior
      : message
  })
}
