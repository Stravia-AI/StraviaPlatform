import type { DisplayCurrency, MoneyAmount } from './money'

export interface StatsWindow {
  /** Paired UTC epoch milliseconds, covering [start_at, end_at). */
  start_at: number
  end_at: number
}

export interface GatewayStatus {
  status: string
  version: string
}

export interface StatsOverview {
  total_requests: number
  /** 已完成或最终失败、非取消、且不是错误的请求。 */
  terminal_request_count: number
  /** 已终结请求中最终失败的请求。 */
  failed_request_count: number
  /** 已得到上游判定结果的 Target 尝试。 */
  upstream_request_count: number
  /** 上游来源的终态失败尝试。 */
  upstream_error_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  total_cache_read_tokens: number | null
  total_cache_write_tokens: number | null
  total_reasoning_tokens: number | null
  avg_duration_ms: number | null
  avg_first_token_ms: number | null
  /** 成功 Target attempts 的总输出 / 总完整耗时（tok/s）；任一样本缺输出或耗时为 null。 */
  avg_output_tps: number | null
  currency: DisplayCurrency
  actual_cost: MoneyAmount
  /** 窗口内存在订阅生效段时的整体订阅对比；否则为 null。 */
  subscription: SubscriptionValue | null
}

export interface StatsSeries {
  /** Effective presentation grain after whole-range coarsening, in milliseconds. */
  bucket_ms: number
  /** Absolute presentation bucket start (epoch ms), anchored to the selected start. */
  bucket_start: number
  request_count: number
  terminal_request_count: number
  failed_request_count: number
  upstream_request_count: number
  upstream_error_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  total_cache_read_tokens: number | null
  total_cache_write_tokens: number | null
  total_reasoning_tokens: number | null
  avg_duration_ms: number | null
  avg_first_token_ms: number | null
  /** 与 overview 相同的完整覆盖口径，仅统计该时间桶的成功 Target attempts。 */
  avg_output_tps: number | null
  currency: DisplayCurrency
  actual_cost: MoneyAmount
}

export interface ModelStats {
  model_id: string | null
  deleted: boolean
  model: string
  request_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  total_reasoning_tokens: number | null
  avg_duration_ms: number | null
  currency: DisplayCurrency
  cost: MoneyAmount
  billing: MoneyAmount
}

export interface ProviderStats {
  provider_id: string | null
  deleted: boolean
  provider: string
  /** 该供应商的 Target 尝试数；重试会使其高于客户端请求数。 */
  upstream_request_count: number
  /** 该供应商来源的上游错误尝试数。 */
  upstream_error_count: number
  avg_duration_ms: number | null
  /** 已完成请求的输出 Token 总速（tok/s）；未完整报告输出或耗时为 null。 */
  avg_output_tps: number | null
  currency: DisplayCurrency
  /** 窗口内该供应商所有已记录成本的尝试的按量成本；订阅尝试为等价按量成本。不计算成本且没有订阅时为 null。 */
  cost: MoneyAmount | null
  /** 窗口内与订阅生效段重叠时的订阅对比；否则为 null。 */
  subscription: SubscriptionValue | null
}

/** 订阅供应商在同一窗口内的成本对比，金额与倍数均由 Core 计算。 */
export interface SubscriptionValue {
  /** 按天线性摊销到窗口内的订阅费用。 */
  cost: MoneyAmount
  /** 同一窗口内订阅供应商尝试的等价按量成本；存在未知尝试时只是下限。 */
  usage_equivalent_cost: MoneyAmount
  /** usage_equivalent_cost ÷ cost 的十进制字符串（最多 4 位小数）；订阅费用为 0 或不可表示时为 null。 */
  multiple: string | null
}

export interface ApiKeyStats {
  deleted: boolean
  api_key_id: string
  api_key_name: string
  request_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  cache_read_tokens: number | null
  cache_write_tokens: number | null
  reasoning_tokens: number | null
  last_used_at: number
  currency: DisplayCurrency
  cost: MoneyAmount
  billing: MoneyAmount
}
