export interface GatewayStatus {
  status: string
  version: string
}

export interface StatsOverview {
  total_requests: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  total_cache_read_tokens: number | null
  total_cache_write_tokens: number | null
  total_reasoning_tokens: number | null
  avg_duration_ms: number | null
  avg_first_token_ms: number | null
  error_count: number
}

export interface StatsHourly {
  hour: string
  request_count: number
  error_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  total_cache_read_tokens: number | null
  total_cache_write_tokens: number | null
  total_reasoning_tokens: number | null
  avg_duration_ms: number | null
  avg_first_token_ms: number | null
}

export interface ModelStats {
  model: string
  request_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  total_reasoning_tokens: number | null
  avg_duration_ms: number | null
}

export interface ProviderStats {
  provider: string
  request_count: number
  error_count: number
  avg_duration_ms: number | null
}

export interface ApiKeyStats {
  api_key_id: string
  api_key_name: string
  request_count: number
  total_input_tokens: number | null
  total_output_tokens: number | null
  cache_read_tokens: number | null
  cache_write_tokens: number | null
  reasoning_tokens: number | null
  last_used_at: number
}
