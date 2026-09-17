import type { InteractionSummary } from '$lib/types'

// 交互链路成员规则（CONTEXT.md「交互链路」）：从未向客户端交付任何可见输出且最终失败的
// 交互不是链路成员，其失败事实归宿于「失败的请求」视图；取消、断线与观察丢失仍保留。
const LIVE_STATUSES: Record<string, true> = { running: true, waiting_client: true }

// 含失败 run 的交互按「失败的请求」口径呈现为失败；运行中/等待客户端保持过程状态。
export function interactionDisplayStatus(interaction: Pick<InteractionSummary, 'status' | 'failed_request'>): string {
  if (interaction.failed_request && !LIVE_STATUSES[interaction.status]) return 'failed'
  return interaction.status
}

export function hiddenFailureNode(interaction: InteractionSummary): boolean {
  return interaction.failed_request && !interaction.client_output_delivered && !LIVE_STATUSES[interaction.status]
}
