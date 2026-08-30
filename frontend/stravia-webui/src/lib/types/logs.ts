export interface RequestLog {
  id: string
  /** Unix 毫秒时间戳 */
  created_at: number
  api_key_id?: string
  api_key_name?: string

  client_protocol?: string
  upstream_protocol?: string
  provider_id?: string
  provider_name?: string
  model_id?: string
  model_name?: string
  upstream_url?: string
  client_model?: string
  upstream_model?: string

  method?: string
  path?: string

  client_request_headers?: string
  client_request_body?: string
  client_response_headers?: string
  client_response_body?: string

  upstream_request_headers?: string
  upstream_request_body?: string
  upstream_response_headers?: string
  upstream_response_body?: string

  upstream_status_code?: number
  client_status_code?: number

  latency_total_ms?: number
  latency_upstream_ms?: number
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  thinking_level?: string

  is_stream: boolean
  stream_chunks_count: number
  stream_first_chunk_ms?: number
}

export interface LogPage {
  items: RequestLog[]
  total: number
}

export interface GatewayStatus {
  status: string
}

export interface StatsOverview {
  total_requests: number
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read_tokens: number
  total_cache_write_tokens: number
  avg_duration_ms: number
  avg_first_token_ms: number | null
  error_count: number
}

export interface StatsHourly {
  hour: string
  request_count: number
  error_count: number
  total_input_tokens: number
  total_output_tokens: number
  total_cache_read_tokens: number
  total_cache_write_tokens: number
  avg_duration_ms: number
  avg_first_token_ms: number | null
}

export interface ModelStats {
  model: string
  request_count: number
  total_input_tokens: number
  total_output_tokens: number
  avg_duration_ms: number
}

export interface ProviderStats {
  provider: string
  request_count: number
  error_count: number
  avg_duration_ms: number
}

export interface ApiKeyStats {
  api_key_id: string
  api_key_name: string
  request_count: number
  total_input_tokens: number
  total_output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  last_used_at: number
}

export interface LogQuery {
  limit?: number
  offset?: number
  provider?: string
  model?: string
  status_min?: number
  status_max?: number
  api_key?: string
}
