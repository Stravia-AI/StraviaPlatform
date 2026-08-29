export interface ApiKey {
  id: string
  key: string
  name: string
  concurrency_limit: number | null
  is_enabled: boolean
  mcp_access_enabled: boolean
  transparent_injection_enabled: boolean
  inject_web_search: boolean
  inject_media_understanding: boolean
  expires_at?: string | null
  created_at: string
  updated_at: string
  model_ids: string[]
}

export interface CreateApiKey {
  key?: string
  name: string
  concurrency_limit?: number | null
  mcp_access_enabled: boolean
  transparent_injection_enabled: boolean
  inject_web_search: boolean
  inject_media_understanding: boolean
  expires_at?: string
  model_ids: string[]
}

export interface UpdateApiKey {
  key?: string
  name?: string
  concurrency_limit?: number | null
  is_enabled?: boolean
  mcp_access_enabled?: boolean
  transparent_injection_enabled?: boolean
  inject_web_search?: boolean
  inject_media_understanding?: boolean
  expires_at?: string
  model_ids?: string[]
}
