import type { InteractionSummary } from '$lib/types'

// 交互链路成员规则（CONTEXT.md「交互链路」）：从未向客户端交付任何可见输出且最终失败的
// 交互不是链路成员，其失败事实归宿于「失败的请求」视图；取消、断线与观察丢失仍保留。
const LIVE_STATUSES: Record<string, true> = { running: true, waiting_client: true }

// failed_request 表示交互历史中出现过失败请求，不覆盖已完成或仍在推进的交互主状态。
// 交互最终停在 interrupted 等非完成终态时，失败仍是主状态。
export function interactionDisplayStatus(interaction: Pick<InteractionSummary, 'status' | 'failed_request'>): string {
  if (interaction.failed_request && interaction.status !== 'completed' && !LIVE_STATUSES[interaction.status]) {
    return 'failed'
  }
  return interaction.status
}

export function hasHistoricalFailure(interaction: Pick<InteractionSummary, 'status' | 'failed_request'>): boolean {
  return interaction.failed_request && interactionDisplayStatus(interaction) !== 'failed'
}

export function hiddenFailureNode(interaction: InteractionSummary): boolean {
  return interaction.failed_request && !interaction.client_output_delivered && !LIVE_STATUSES[interaction.status]
}
