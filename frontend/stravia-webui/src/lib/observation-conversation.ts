import type { InteractionDetail } from './types/observation'

export interface ObservationChatMessage {
  id: string
  role: 'user' | 'assistant'
  text: string
  at: number
  model: string
  live: boolean
}

export function observationConversationMessages(detail: InteractionDetail): ObservationChatMessage[] {
  const interaction = detail.interaction
  const model = interaction.first_model_display_name?.trim() || interaction.first_route_id
  const messages: ObservationChatMessage[] = [
    {
      id: `user:${interaction.id}`,
      role: 'user',
      text: interaction.input_preview ?? '',
      at: interaction.started_at,
      model: '',
      live: false,
    },
  ]
  const runs = [...detail.runs].sort((left, right) => left.started_at - right.started_at)
  let hasVisibleOutput = false
  for (const run of runs) {
    // 只使用已交付的公开文本；checkpoint、工具参数及 Debug payload 不是聊天回复。
    const chunks = run.events
      .filter((event) => event.kind === 'client_visible_content_delta' && event.run_id === run.id)
      .sort((left, right) => left.sequence - right.sequence)
      .flatMap((event) => {
        if (!event.payload || typeof event.payload !== 'object' || !('text' in event.payload)) return []
        return typeof event.payload.text === 'string' ? [event.payload.text] : []
      })
    const text = chunks.join('')
    hasVisibleOutput ||= text.length > 0
    messages.push({
      id: `assistant:${run.id}`,
      role: 'assistant',
      text,
      at: run.started_at,
      model: run.model_display_name?.trim() || run.route_id,
      live: run.status === 'running',
    })
  }
  if (runs.length === 0) {
    messages.push({
      id: `assistant:${interaction.id}`,
      role: 'assistant',
      text: '',
      at: interaction.started_at,
      model,
      live: interaction.status === 'running',
    })
  }
  // 旧记录缺少正文事件时只回退一次；不能把整个交互的尾部复制到每个 Run。
  if (!hasVisibleOutput && interaction.visible_tail) {
    const last = messages[messages.length - 1]
    last.text = interaction.visible_tail
  }
  return messages
}
